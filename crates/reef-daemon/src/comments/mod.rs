//! Comments on changes, and instructions to the session that made them
//! (R3.12, ADR 0069; `docs/comments.md`).
//!
//! A comment is an immutable note or instruction on a change. Every write
//! is recorded the way R3.09 records a transition: pending rows in
//! `comments` and `comment_events`, then a jj operation whose
//! `reef.comment` attribute carries the events (ids and states, never the
//! text), then the rows committed with the operation's id. A crash between
//! the two leaves pending rows that `resolve_pending` commits or drops at
//! the next start.
//!
//! An instruction is written awaiting confirmation with an estimate of what
//! Reef recorded ([`estimate`]), routed to the session that proposed the
//! change ([`target`]), and taken by that session through [`claim`]; Reef
//! starts no agent. A re-proposal of the change finishes the instructions
//! delivered before it, a final change cancels unconfirmed and pending
//! ones ([`after_transitions`]), and instructions left untouched for 7 days
//! expire ([`expire`]). Every write of a repository runs under its comment
//! lock, taken after the create lock shared and never with the lifecycle
//! lock held.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::FutureExt as _;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::comments::{self as jj_comments, CommentEventRecord};
use reef_lib::jj::lifecycle::LifecycleError as JjRecordError;
use reef_lib::jj::log::{ChangeInfo, Describer};
use reef_lib::jj::oplog::{self, OpMeta, RemoteToken};
use reef_lib::jj::revset::{self, RevisionError, RevsetScope};
use reef_lib::jj::rewrite::{self, RewriteError};
use reef_lib::jj::{CommitIdHex, OpKind, OperationIdHex};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::Instrument as _;

use crate::daemon::AppState;
use crate::db::{
    ClaimQuery, CommentEventRow, CommentRow, DbError, InstructionColumns, TransitionRow,
};
use crate::error::{Advice, Advise};
use crate::lifecycle::Actor;
use crate::model::{RepoId, Timestamp, WorkspaceRecord};
use crate::repo::RepoThreadError;
use crate::workspace::ManagerError;

pub mod messages;

/// A comment's text, at most this many bytes.
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
/// An instruction's summary, at most this many bytes.
pub const MAX_SUMMARY_BYTES: usize = 4096;
/// The most comments a change has.
pub const MAX_COMMENTS_PER_CHANGE: u64 = 1000;
/// The most unfinished instructions a change has.
pub const MAX_OPEN_PER_CHANGE: u64 = 16;
/// The most unfinished instructions a target session has.
pub const MAX_OPEN_PER_SESSION: u64 = 64;
/// How long an unfinished instruction may be left untouched.
pub const EXPIRY: Duration = Duration::from_hours(24 * 7);
/// How often the sweep looks for expired instructions.
const SWEEP_EVERY: Duration = Duration::from_mins(10);
/// How many earlier instructions the agent-time estimate looks at.
const AGENT_SAMPLES: usize = 50;
/// The most replies `GetComment` returns.
pub const MAX_REPLIES: usize = 100;
/// The most instructions one expiry pass ends.
const EXPIRE_BATCH: usize = 500;
/// How long before a pending event's creation its operation may have ended
/// and still be found at start.
const PENDING_SLACK: Duration = Duration::from_mins(5);

/// What a comment is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Data for whoever reads it.
    Note,
    /// Work for the change's session.
    Instruction,
}

impl Kind {
    /// The column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Note => "note",
            Kind::Instruction => "instruction",
        }
    }
}

/// An instruction's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    /// Written with its estimate, not yet confirmed.
    AwaitingConfirmation,
    /// Confirmed, waiting for its session.
    Pending,
    /// Taken by a client for a session.
    Delivered,
    /// Started.
    Running,
    /// Finished. Final.
    Done,
    /// Could not be done. Final.
    Failed,
    /// Withdrawn or made moot. Final.
    Cancelled,
}

impl State {
    /// Every state, in order.
    pub const ALL: [State; 7] = [
        State::AwaitingConfirmation,
        State::Pending,
        State::Delivered,
        State::Running,
        State::Done,
        State::Failed,
        State::Cancelled,
    ];

    /// The column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            State::AwaitingConfirmation => "awaiting_confirmation",
            State::Pending => "pending",
            State::Delivered => "delivered",
            State::Running => "running",
            State::Done => "done",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
        }
    }

    /// The state a column's text names.
    pub fn parse(text: &str) -> Option<State> {
        State::ALL.into_iter().find(|state| state.as_str() == text)
    }

    /// Whether nothing leaves it.
    pub fn is_final(self) -> bool {
        matches!(self, State::Done | State::Failed | State::Cancelled)
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Whether a client's `UpdateInstruction` may move an instruction from
/// `from` to `to`. A claim moves PENDING to DELIVERED or RUNNING; the
/// daemon's own moves (reproposed, change final, expired) are its.
pub fn allowed(from: State, to: State) -> bool {
    use State as S;
    match from {
        S::AwaitingConfirmation => matches!(to, S::Pending | S::Cancelled),
        S::Pending => matches!(to, S::Cancelled),
        S::Delivered => matches!(
            to,
            S::Pending | S::Running | S::Done | S::Failed | S::Cancelled
        ),
        S::Running => matches!(to, S::Done | S::Failed),
        S::Done | S::Failed | S::Cancelled => false,
    }
}

/// The event a move records, which its operation's description names.
pub fn event_of(from: State, to: State) -> &'static str {
    match (from, to) {
        (State::AwaitingConfirmation, State::Pending) => "confirm",
        (State::Delivered, State::Pending) => "release",
        (State::Pending, State::Delivered | State::Running) => "claim",
        (_, State::Running) => "start",
        (_, State::Cancelled) => "cancel",
        _ => "finish",
    }
}

/// Whether `text` may be recorded as a comment's text (`max` bytes) or a
/// summary: not empty when `required`, at most `max` bytes, and no control
/// character but newline and tab.
pub fn valid_text(text: &str, max: usize, required: bool) -> bool {
    // Bidirectional overrides and isolates reorder what a terminal or a
    // reader shows, so they are refused like control characters.
    let bidi = |c: char| matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    (!required || !text.trim().is_empty())
        && text.len() <= max
        && !text
            .chars()
            .any(|c| (c.is_control() && c != '\n' && c != '\t') || bidi(c))
}

/// SHA-256 of `bytes`, in hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    ring::digest::digest(&ring::digest::SHA256, bytes)
        .as_ref()
        .iter()
        .fold(String::new(), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Transitions a repository committed, handed to the sweep.
type Noted = (RepoId, Vec<TransitionRow>);

/// The per-repository comment locks, and the queue of transitions the
/// sweep acts on; one in [`AppState`].
#[derive(Debug)]
pub struct Comments {
    locks: Mutex<HashMap<RepoId, Arc<tokio::sync::Mutex<()>>>>,
    sender: mpsc::UnboundedSender<Noted>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<Noted>>>,
}

impl Default for Comments {
    fn default() -> Self {
        let (sender, receiver) = mpsc::unbounded_channel();
        Comments {
            locks: Mutex::default(),
            sender,
            receiver: Mutex::new(Some(receiver)),
        }
    }
}

impl Comments {
    /// `repo`'s comment lock: held from a write's checks to its commit.
    pub fn lock(&self, repo: RepoId) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(locks.entry(repo).or_default())
    }

    /// Hands committed transitions to the sweep, which finishes or cancels
    /// the instructions they make moot ([`after_transitions`]). Called by
    /// the lifecycle's record path, which holds the lifecycle lock, so the
    /// work happens later, on the sweep's task.
    pub fn note_transitions(&self, repo: RepoId, rows: &[TransitionRow]) {
        let relevant: Vec<TransitionRow> =
            rows.iter().filter(|row| matters(row)).cloned().collect();
        if !relevant.is_empty() {
            let _ = self.sender.send((repo, relevant));
        }
    }
}

/// Whether a transition moves any instruction: a request to propose, or a
/// change landing or superseded.
fn matters(row: &TransitionRow) -> bool {
    (row.to_state == "proposed" && row.cause == "request")
        || row.to_state == "landed"
        || row.to_state == "superseded"
}

/// Why a comment write or read was refused or failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum CommentError {
    /// No kind.
    #[error("say whether the comment is a note or an instruction")]
    KindRequired,
    /// Text that is empty, too long, or has a control character.
    #[error(
        "the text is empty, over its limit, or has a control character other than newline and tab"
    )]
    InvalidText,
    /// An anchor that names nothing in the tree.
    #[error("{path:?} is not a file with those lines in the anchor's tree")]
    InvalidAnchor {
        /// The path.
        path: String,
    },
    /// A reply to no comment on the change.
    #[error("{reply_to:?} is not a comment on this change")]
    InvalidReplyTo {
        /// What it named.
        reply_to: String,
    },
    /// `confirm` without ceilings, or on a note.
    #[error("confirming an instruction in the same call needs ceilings")]
    CeilingsRequired,
    /// No target state, or one a request may not set.
    #[error("name the state to move the instruction to")]
    StateRequired,
    /// No such comment.
    #[error("no comment {id} in this repository")]
    NotFound {
        /// The id.
        id: String,
    },
    /// A move or claim of a note.
    #[error("comment {id} is a note, not an instruction")]
    NotAnInstruction {
        /// The id.
        id: String,
    },
    /// The table does not allow the move.
    #[error("the instruction is {state}; it cannot become {to}")]
    NotAllowed {
        /// Its state.
        state: State,
        /// The target.
        to: State,
    },
    /// `expected_state` is not the instruction's.
    #[error("the instruction is {state}, not the expected {expected}")]
    ExpectedState {
        /// What the caller expected.
        expected: State,
        /// What it is.
        state: State,
    },
    /// An instruction on a change that has landed or was superseded.
    #[error("change {change} is {state}; an instruction there could only expire")]
    FinalChange {
        /// The change.
        change: String,
        /// `landed` or `superseded`.
        state: String,
    },
    /// A claim with nothing to match on.
    #[error("name the workspace, client session or change to take an instruction for")]
    ClaimScopeRequired,
    /// Too many comments on the change.
    #[error("the change has {max} comments already")]
    TooManyComments {
        /// The limit.
        max: u64,
    },
    /// Too many unfinished instructions.
    #[error("{max} instructions are unfinished already")]
    TooManyInstructions {
        /// The limit.
        max: u64,
    },
    /// A remote instruction or take from a token linked to no identity.
    #[error("through the remote listener, this needs a token linked to an operator identity")]
    RemoteIdentityRequired,
    /// A remote move or claim of another token's.
    #[error("comment {id} was not written or claimed through this remote token")]
    RemoteNotOwned {
        /// The comment, or the change for a claim.
        id: String,
    },
    /// The revision could not be resolved.
    #[error("resolve the revision")]
    Revision(#[source] RevisionError),
    /// The commit cannot be commented on as named.
    #[error("check the commit")]
    Rewrite(#[source] RewriteError),
    /// A file of the anchor could not be read.
    #[error("read the anchor's file")]
    File(#[source] reef_lib::jj::files::FileError),
    /// The repository could not be reached or refreshed.
    #[error("reach the repository")]
    Manager(#[source] ManagerError),
    /// The repository thread failed.
    #[error("run on the repository thread")]
    Thread(#[source] RepoThreadError),
    /// The operation could not be written.
    #[error("record the comment")]
    Record(#[source] JjRecordError),
    /// The database failed.
    #[error("read or write comment rows")]
    Db(#[source] DbError),
}

macro_rules! from_error {
    ($($source:ty => $variant:ident),* $(,)?) => {
        $(impl From<$source> for CommentError {
            fn from(error: $source) -> Self {
                CommentError::$variant(error)
            }
        })*
    };
}

from_error! {
    RevisionError => Revision,
    RewriteError => Rewrite,
    reef_lib::jj::files::FileError => File,
    ManagerError => Manager,
    RepoThreadError => Thread,
    JjRecordError => Record,
    DbError => Db,
}

impl From<reef_lib::jj::OpenError> for CommentError {
    fn from(error: reef_lib::jj::OpenError) -> Self {
        CommentError::Manager(ManagerError::from(error))
    }
}

impl Classify for CommentError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::KindRequired
            | Self::InvalidText
            | Self::InvalidAnchor { .. }
            | Self::InvalidReplyTo { .. }
            | Self::CeilingsRequired
            | Self::StateRequired => ErrorKind::InvalidArgument,
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::NotAnInstruction { .. }
            | Self::NotAllowed { .. }
            | Self::ExpectedState { .. }
            | Self::FinalChange { .. }
            | Self::ClaimScopeRequired => ErrorKind::Conflict,
            Self::TooManyComments { .. } | Self::TooManyInstructions { .. } => {
                ErrorKind::Unavailable
            }
            Self::RemoteIdentityRequired | Self::RemoteNotOwned { .. } => {
                ErrorKind::PermissionDenied
            }
            Self::Revision(error) => error.kind(),
            Self::Rewrite(error) => error.kind(),
            Self::File(error) => error.kind(),
            Self::Manager(error) => error.kind(),
            Self::Thread(error) => error.kind(),
            Self::Record(error) => error.kind(),
            Self::Db(error) => error.kind(),
        }
    }
}

impl CommentError {
    /// Its reason and metadata, as `crate::error` reports them.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            Self::KindRequired => (reason::KIND_REQUIRED, Vec::new()),
            Self::InvalidText => (reason::INVALID_TEXT, Vec::new()),
            Self::InvalidAnchor { path } => (reason::INVALID_ANCHOR, vec![("path", path.clone())]),
            Self::InvalidReplyTo { reply_to } => (
                reason::INVALID_REPLY_TO,
                vec![("reply_to", reply_to.clone())],
            ),
            Self::CeilingsRequired => (reason::CEILINGS_REQUIRED, Vec::new()),
            Self::StateRequired => (reason::INSTRUCTION_STATE_REQUIRED, Vec::new()),
            Self::NotFound { id } => (reason::COMMENT_NOT_FOUND, vec![("id", id.clone())]),
            Self::NotAnInstruction { id } => (reason::NOT_AN_INSTRUCTION, vec![("id", id.clone())]),
            Self::NotAllowed { state, to } => (
                reason::INSTRUCTION_NOT_ALLOWED,
                vec![
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
            Self::ClaimScopeRequired => (reason::CLAIM_SCOPE_REQUIRED, Vec::new()),
            Self::FinalChange { change, state } => (
                reason::CHANGE_FINAL,
                vec![("change_id", change.clone()), ("state", state.clone())],
            ),
            Self::TooManyComments { max } => {
                (reason::TOO_MANY_COMMENTS, vec![("max", max.to_string())])
            }
            Self::TooManyInstructions { max } => (
                reason::TOO_MANY_INSTRUCTIONS,
                vec![("max", max.to_string())],
            ),
            Self::RemoteIdentityRequired => (reason::REMOTE_IDENTITY_REQUIRED, Vec::new()),
            Self::RemoteNotOwned { id } => {
                (reason::REMOTE_COMMENT_NOT_OWNED, vec![("id", id.clone())])
            }
            _ => return None,
        })
    }
}

impl Advise for CommentError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::KindRequired => Some(Advice::hint(
                "Write a note (data for whoever reads it) or an instruction (work for the session that made the change).",
            )),
            Self::InvalidAnchor { .. } => Some(Advice::hint(
                "Name a file in the commit (or, with the old side, in its first parent) and lines inside it.",
            )),
            Self::CeilingsRequired => Some(Advice::hint(
                "Confirm in one call only with ceilings the estimate must meet; otherwise write the instruction, read its estimate, then confirm it.",
            )),
            Self::NotFound { .. } => Some(Advice::hint(
                "List the change's comments with `reef comment list`.",
            )),
            Self::NotAllowed { state, .. } if state.is_final() => Some(Advice::hint(format!(
                "The instruction is {state}, which is final; write a new one."
            ))),
            Self::NotAllowed { .. } | Self::ExpectedState { .. } => Some(Advice::hint(
                "Read the instruction again (`reef comment show <id>`) and move it from the state it is in.",
            )),
            Self::ClaimScopeRequired => Some(Advice::hint(
                "Run it in the session's workspace, or name its client session or the change.",
            )),
            Self::TooManyInstructions { .. } => Some(Advice::hint(
                "Finish or cancel some of the unfinished instructions first.",
            )),
            Self::RemoteIdentityRequired => Some(Advice::hint(
                "Use a remote token linked to an operator identity (`reef remote token create --identity`).",
            )),
            Self::RemoteNotOwned { .. } => Some(Advice::hint(
                "Through the remote listener, only the token that wrote an instruction confirms or cancels it, only the token that claimed it moves it after, and only the token that proposed a change claims its instructions without taking them.",
            )),
            _ => None,
        }
    }
}

/// The estimate of an instruction's cost, as stored (JSON) and reported:
/// what Reef recorded, never tokens or price.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Estimate {
    /// SHA-256 of this JSON with `digest` empty and `computed_at` 0.
    #[serde(default)]
    pub digest: String,
    /// When it was computed, in milliseconds since the epoch.
    #[serde(default)]
    pub computed_at: i64,
    /// Whether any check finished on the tree.
    pub checks_known: bool,
    /// How many.
    pub checks: u64,
    /// Their wall time.
    pub check_wall_ms: u64,
    /// Their CPU time.
    pub check_cpu_ms: u64,
    /// Whether earlier instructions give an agent time.
    pub agent_known: bool,
    /// The median delivery-to-done wall time.
    pub agent_wall_ms: u64,
    /// Of how many.
    pub agent_samples: u32,
    /// `change`, `session` or `repository`.
    pub agent_basis: String,
    /// The target's declared model.
    pub model: String,
    /// The target's declared harness.
    pub harness: String,
    /// What is unknown, a sentence each.
    pub unknown: Vec<String>,
}

impl Estimate {
    /// Sets `digest` from the other fields.
    fn seal(&mut self, at: Timestamp) {
        let mut bare = self.clone();
        bare.digest = String::new();
        bare.computed_at = 0;
        self.digest = sha256_hex(serde_json::to_string(&bare).unwrap_or_default().as_bytes());
        self.computed_at = at.0;
    }
}

/// The ceilings a one-call confirmation checks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Ceilings {
    /// The most check wall time, in milliseconds; 0 for no bound.
    pub max_check_wall_ms: u64,
    /// The most agent wall time; 0 for no bound.
    pub max_agent_wall_ms: u64,
    /// Whether an unknown part meets its ceiling.
    pub allow_unknown: bool,
}

/// Why `estimate` does not meet `ceilings`, if it does not.
pub fn ceiling_refusal(estimate: &Estimate, ceilings: &Ceilings) -> Option<String> {
    if ceilings.max_check_wall_ms > 0 {
        if !estimate.checks_known && !ceilings.allow_unknown {
            return Some("the checks' time is unknown and allow_unknown is not set".to_owned());
        }
        if estimate.checks_known && estimate.check_wall_ms > ceilings.max_check_wall_ms {
            return Some(format!(
                "the checks took {} ms, over the ceiling of {} ms",
                estimate.check_wall_ms, ceilings.max_check_wall_ms
            ));
        }
    }
    if ceilings.max_agent_wall_ms > 0 {
        if !estimate.agent_known && !ceilings.allow_unknown {
            return Some("the agent's time is unknown and allow_unknown is not set".to_owned());
        }
        if estimate.agent_known && estimate.agent_wall_ms > ceilings.max_agent_wall_ms {
            return Some(format!(
                "earlier instructions took {} ms of agent time, over the ceiling of {} ms",
                estimate.agent_wall_ms, ceilings.max_agent_wall_ms
            ));
        }
    }
    None
}

/// The sentence every estimate carries while no runner reports tokens.
pub const TOKENS_UNKNOWN: &str = "model tokens and price are not visible to Reef";

/// Computes the estimate for an instruction on `change`, whose checks ran
/// on `tree` and whose target declared `client_session`, `model` and
/// `harness`.
pub fn estimate(
    state: &AppState,
    repo: RepoId,
    change: &str,
    tree: Option<&str>,
    target: Option<&TargetFacts>,
) -> Result<Estimate, DbError> {
    let mut estimate = Estimate::default();
    let (checks, wall, cpu) = match tree {
        Some(tree) => state.db.job_usage_on_tree(repo, tree)?,
        None => (0, 0, 0),
    };
    if checks > 0 {
        estimate.checks_known = true;
        estimate.checks = checks;
        estimate.check_wall_ms = wall;
        estimate.check_cpu_ms = cpu;
    } else {
        estimate
            .unknown
            .push("no check of this change's tree has finished".to_owned());
    }
    let session = target.and_then(|target| target.client_session.as_deref());
    match state
        .db
        .instruction_agent_time(repo, change, session, AGENT_SAMPLES)?
    {
        Some((median, samples, basis)) => {
            estimate.agent_known = true;
            estimate.agent_wall_ms = median;
            estimate.agent_samples = samples;
            basis.clone_into(&mut estimate.agent_basis);
        }
        None => estimate
            .unknown
            .push("no earlier instruction in this repository has finished".to_owned()),
    }
    if let Some(target) = target {
        estimate.model = target.model.clone().unwrap_or_default();
        estimate.harness = target.harness.clone().unwrap_or_default();
    }
    if estimate.model.is_empty() {
        estimate
            .unknown
            .push("the target session declared no model".to_owned());
    }
    estimate.unknown.push(TOKENS_UNKNOWN.to_owned());
    estimate.seal(Timestamp::now());
    Ok(estimate)
}

/// The session an instruction is for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TargetFacts {
    /// `proposal` or `provenance`.
    pub source: &'static str,
    /// The provenance session.
    pub session_id: String,
    /// Its declared client session.
    pub client_session: Option<String>,
    /// Its declared harness.
    pub harness: Option<String>,
    /// Its declared model.
    pub model: Option<String>,
    /// Its agent.
    pub agent: Option<String>,
    /// Its workspace.
    pub workspace_id: String,
}

/// The session an instruction on `change` is for (R3.12's routing): the
/// one that recorded its latest proposal, else its latest provenance
/// record's; `None` when neither.
pub fn target(
    state: &AppState,
    repo: RepoId,
    change: &str,
) -> Result<Option<TargetFacts>, DbError> {
    let Some((session_id, source)) = state.db.instruction_target_session(repo, change)? else {
        return Ok(None);
    };
    let Some(session) = state.db.provenance_session(&session_id)? else {
        return Ok(None);
    };
    Ok(Some(TargetFacts {
        source,
        session_id,
        client_session: session.declared.client_session,
        harness: session.declared.harness,
        model: session.declared.model,
        agent: session.agent,
        workspace_id: session.workspace_id,
    }))
}

/// A comment's anchor as requested.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Anchor {
    /// The path.
    pub path: String,
    /// In the first parent's tree.
    pub old: bool,
    /// The first line; 0 for the whole file.
    pub start_line: u32,
    /// The last line; 0 for `start_line` alone.
    pub end_line: u32,
}

/// A client's comment, resolved by the service.
#[derive(Clone, Debug)]
pub struct CreateRequest {
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
    /// The commit the caller saw; empty for no check.
    pub expected_commit: String,
    /// Where in the change.
    pub anchor: Option<Anchor>,
    /// The comment it replies to; empty for none.
    pub reply_to: String,
    /// What it is; `None` is refused.
    pub kind: Option<Kind>,
    /// The text.
    pub text: String,
    /// The declared actor.
    pub actor: Option<Actor>,
    /// Confirm in this call, within `ceilings`.
    pub confirm: bool,
    /// The ceilings.
    pub ceilings: Option<Ceilings>,
    /// The call's agent, marked with its remote token.
    pub agent: Option<String>,
    /// The remote token, `(id, name)`.
    pub remote: Option<(String, String)>,
}

/// What [`create`] wrote.
#[derive(Debug)]
pub struct CreateOutcome {
    /// The comment.
    pub comment: CommentRow,
    /// The operation.
    pub op_id: OperationIdHex,
    /// Why `confirm` did not confirm.
    pub confirm_refused: Option<String>,
}

/// An event about to be recorded, with the comment it creates.
struct Planned {
    event: CommentEventRow,
    comment: Option<CommentRow>,
    kind: &'static str,
    round: u32,
}

impl Planned {
    fn record(&self) -> CommentEventRecord {
        let event = &self.event;
        CommentEventRecord {
            id: event.id.clone(),
            comment: event.comment_id.clone(),
            change: event.change_id.clone(),
            round: self.round,
            kind: self.kind.to_owned(),
            event: event.event.clone(),
            from: event.from_state.clone().unwrap_or_default(),
            to: event.to_state.clone().unwrap_or_default(),
            cause: event.cause.clone(),
        }
    }
}

/// Records `planned` in one operation described by `meta`: pending rows,
/// the operation, then the rows committed. The caller holds the
/// repository's create lock shared and its comment lock.
async fn record(
    state: &AppState,
    repo: RepoId,
    planned: Vec<Planned>,
    mut meta: OpMeta,
) -> Result<(OperationIdHex, Vec<CommentEventRow>), CommentError> {
    for plan in &planned {
        match &plan.comment {
            Some(comment) => state.db.insert_pending_comment(comment, &plan.event)?,
            None => state.db.insert_pending_comment_event(&plan.event)?,
        }
    }
    let drop_pending = |state: &AppState| {
        for plan in &planned {
            if let Err(error) = state.db.delete_pending_comment_event(repo, &plan.event.id) {
                tracing::warn!(error = %ErrorChain(&error), "cannot drop a pending comment event; the next start does");
            }
        }
    };
    meta.comment_events = planned.iter().map(Planned::record).collect();
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
                let op_id = head.repo.record_comment_events(&meta).await?;
                let end = oplog::entry(head.repo.readonly().operation()).end;
                Ok::<_, CommentError>((op_id, end))
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
            tracing::warn!(error = %ErrorChain(&error), "the repository thread failed during a comment write; the next start resolves its pending rows");
            return Err(error.into());
        }
    };
    let recorded_at = Timestamp::from(end);
    let mut rows = Vec::with_capacity(planned.len());
    for plan in &planned {
        if let Some(row) =
            state
                .db
                .commit_comment_event(repo, &plan.event.id, op_id.as_str(), recorded_at)?
        {
            rows.push(row);
        }
    }
    Ok((op_id, rows))
}

/// The agent a request carries, and its remote token's name.
fn agent_parts(agent: Option<&str>) -> (Option<String>, Option<String>) {
    match agent {
        Some(marked) => {
            let (agent, token) = RemoteToken::unmark(marked);
            (Some(agent.to_owned()), token.map(|token| token.name))
        }
        None => (None, None),
    }
}

/// Whether the remote token `token` is linked to an operator identity.
fn token_linked(state: &AppState, token: &str) -> bool {
    state
        .db
        .remote_tokens()
        .ok()
        .and_then(|rows| rows.into_iter().find(|row| row.id == token))
        .and_then(|row| row.identity_id)
        .is_some()
}

/// What a comment's checks found on the repository thread.
struct Facts {
    info: ChangeInfo,
    /// The tree of the change's recorded proposal commit, when it has one.
    proposal_tree: Option<String>,
}

/// A comment's checks on the repository thread: the revision, the expected
/// commit, the anchor, and the proposal's tree.
async fn resolve(
    head: &mut crate::repo::RepoHead,
    request: &CreateRequest,
) -> Result<Facts, CommentError> {
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
    if info.hidden {
        return Err(RewriteError::Hidden(info.commit_id).into());
    }
    if let Some(anchor) = &request.anchor {
        let invalid = || CommentError::InvalidAnchor {
            path: anchor.path.clone(),
        };
        let file = jj_comments::anchor_file(&repo, &info.commit_id, anchor.old, &anchor.path)
            .await?
            .ok_or_else(invalid)?;
        if anchor.start_line > 0 {
            let end = anchor.end_line.max(anchor.start_line);
            if file.binary || file.lines.is_some_and(|lines| u64::from(end) > lines) {
                return Err(invalid());
            }
        }
    }
    Ok(Facts {
        info,
        proposal_tree: None,
    })
}

/// Checks what needs no repository.
fn check_create(request: &CreateRequest) -> Result<Kind, CommentError> {
    let kind = request.kind.ok_or(CommentError::KindRequired)?;
    if !valid_text(&request.text, MAX_TEXT_BYTES, true) {
        return Err(CommentError::InvalidText);
    }
    // A one-call confirmation needs at least one ceiling that bounds
    // something: 0 means no bound, and an empty set would confirm anything.
    let bounded = request
        .ceilings
        .is_some_and(|ceilings| ceilings.max_check_wall_ms > 0 || ceilings.max_agent_wall_ms > 0);
    if request.confirm && (kind == Kind::Note || !bounded) {
        return Err(CommentError::CeilingsRequired);
    }
    if let Some(anchor) = &request.anchor {
        let bad_lines =
            anchor.end_line > 0 && (anchor.start_line == 0 || anchor.end_line < anchor.start_line);
        if anchor.path.is_empty() || bad_lines {
            return Err(CommentError::InvalidAnchor {
                path: anchor.path.clone(),
            });
        }
    }
    if request.revision.trim().is_empty() {
        return Err(RevisionError::Required.into());
    }
    Ok(kind)
}

/// Writes a comment for a client; see the module docs.
///
/// Runs as its own task, so a caller that stops waiting cannot cut it
/// between its operation and its rows.
pub async fn create(
    state: &Arc<AppState>,
    request: CreateRequest,
) -> Result<CreateOutcome, CommentError> {
    let state = Arc::clone(state);
    let repo = request.repo;
    let span = tracing::Span::current();
    tokio::spawn(async move { create_inner(&state, request).await }.instrument(span))
        .await
        .map_err(|_| CommentError::Thread(RepoThreadError::Panicked { id: repo }))?
}

#[expect(
    clippy::too_many_lines,
    reason = "the checks in their order, then the rows it records"
)]
async fn create_inner(
    state: &AppState,
    request: CreateRequest,
) -> Result<CreateOutcome, CommentError> {
    let kind = check_create(&request)?;
    if let Some((token, _)) = &request.remote
        && kind == Kind::Instruction
        && !token_linked(state, token)
    {
        return Err(CommentError::RemoteIdentityRequired);
    }
    let repo = request.repo;
    let handle = state.manager.handle(repo)?;
    let create_lock = state.manager.repo_lock(repo);
    let _shared = create_lock.read().await;
    let comment_lock = state.comments.lock(repo);
    let _held = comment_lock.lock().await;

    let mut facts = {
        let request = request.clone();
        handle
            .call_async(move |head| async move { resolve(head, &request).await }.boxed_local())
            .await??
    };
    // Checks re-run on the proposal's recorded commit, when the change has
    // one: its tree is what the estimate looks for jobs on.
    let recorded = state
        .db
        .lifecycle_row(repo, facts.info.change_id.as_str())?
        .map(|row| row.commit_id);
    facts.proposal_tree =
        match recorded {
            Some(commit) if commit == facts.info.commit_id.as_str() => facts.info.tree_id.clone(),
            Some(commit) => match commit.parse::<CommitIdHex>() {
                Ok(commit) => handle
                    .call_async(move |head| {
                        async move { jj_comments::tree_of(&head.repo.readonly(), &commit).await }
                            .boxed_local()
                    })
                    .await?,
                Err(_) => None,
            },
            None => None,
        };
    let change = facts.info.change_id.to_string();
    let commit = facts.info.commit_id.to_string();
    if !request.reply_to.trim().is_empty() {
        let parent = state.db.comment(repo, request.reply_to.trim())?;
        if parent.is_none_or(|parent| parent.change_id != change) {
            return Err(CommentError::InvalidReplyTo {
                reply_to: request.reply_to.clone(),
            });
        }
    }
    if state.db.comment_count(repo, &change)? >= MAX_COMMENTS_PER_CHANGE {
        return Err(CommentError::TooManyComments {
            max: MAX_COMMENTS_PER_CHANGE,
        });
    }
    let target = if kind == Kind::Instruction {
        target(state, repo, &change)?
    } else {
        None
    };
    if kind == Kind::Instruction {
        let session = target.as_ref().map(|target| {
            target
                .client_session
                .clone()
                .unwrap_or_else(|| target.session_id.clone())
        });
        let (by_change, by_session) =
            state
                .db
                .open_instruction_counts(repo, &change, session.as_deref())?;
        if by_change >= MAX_OPEN_PER_CHANGE {
            return Err(CommentError::TooManyInstructions {
                max: MAX_OPEN_PER_CHANGE,
            });
        }
        if by_session >= MAX_OPEN_PER_SESSION {
            return Err(CommentError::TooManyInstructions {
                max: MAX_OPEN_PER_SESSION,
            });
        }
    }
    let lifecycle = state.db.lifecycle_row(repo, &change)?;
    if kind == Kind::Instruction {
        let recorded = lifecycle
            .as_ref()
            .map(|row| row.state.clone())
            .filter(|state| state == "landed" || state == "superseded");
        if let Some(final_state) =
            recorded.or_else(|| facts.info.immutable.then(|| "landed".to_owned()))
        {
            return Err(CommentError::FinalChange {
                change,
                state: final_state,
            });
        }
    }
    let round = lifecycle.as_ref().map_or(0, |row| row.round);
    let (agent, remote_name) = agent_parts(request.agent.as_deref());
    let actor = if request.remote.is_some() {
        Actor::Agent
    } else {
        request.actor.unwrap_or(if agent.is_some() {
            Actor::Agent
        } else {
            Actor::Human
        })
    };
    let (identity_id, identity_name, session_id) =
        crate::lifecycle::identity_for(state, request.workspace.as_ref(), request.remote.as_ref());
    let now = Timestamp::now();
    let id = crate::identity::new_id("cmt_");
    let mut confirm_refused = None;
    let instruction = if kind == Kind::Instruction {
        let tree = facts
            .proposal_tree
            .clone()
            .or_else(|| facts.info.tree_id.clone());
        let estimate = estimate(state, repo, &change, tree.as_deref(), target.as_ref())?;
        let initial = if request.confirm {
            let ceilings = request.ceilings.unwrap_or_default();
            match ceiling_refusal(&estimate, &ceilings) {
                None => State::Pending,
                Some(why) => {
                    confirm_refused = Some(why);
                    State::AwaitingConfirmation
                }
            }
        } else {
            State::AwaitingConfirmation
        };
        Some(InstructionColumns {
            state: initial.as_str().to_owned(),
            cause: "request".to_owned(),
            estimate: serde_json::to_string(&estimate).unwrap_or_default(),
            estimate_digest: estimate.digest.clone(),
            target_source: target
                .as_ref()
                .map_or("none", |target| target.source)
                .to_owned(),
            target_session_id: target.as_ref().map(|target| target.session_id.clone()),
            target_client_session: target
                .as_ref()
                .and_then(|target| target.client_session.clone()),
            target_harness: target.as_ref().and_then(|target| target.harness.clone()),
            target_agent: target.as_ref().and_then(|target| target.agent.clone()),
            target_workspace_id: target.as_ref().map(|target| target.workspace_id.clone()),
            ..InstructionColumns::default()
        })
    } else {
        None
    };
    let to_state = instruction.as_ref().map(|columns| columns.state.clone());
    let event_name = if kind == Kind::Note {
        "comment"
    } else {
        "instruct"
    };
    let anchor = request.anchor.clone();
    let comment = CommentRow {
        repo_id: repo,
        id: id.clone(),
        committed: false,
        change_id: change.clone(),
        round,
        commit_id: commit,
        path: anchor.as_ref().map(|anchor| anchor.path.clone()),
        side: anchor
            .as_ref()
            .map(|anchor| if anchor.old { "old" } else { "new" }.to_owned()),
        start_line: anchor
            .as_ref()
            .map(|anchor| anchor.start_line)
            .filter(|line| *line > 0),
        end_line: anchor.as_ref().and_then(|anchor| {
            (anchor.start_line > 0).then(|| anchor.end_line.max(anchor.start_line))
        }),
        reply_to: Some(request.reply_to.trim().to_owned()).filter(|reply| !reply.is_empty()),
        kind: kind.as_str().to_owned(),
        text: request.text.clone(),
        text_sha256: sha256_hex(request.text.as_bytes()),
        agent: agent.clone(),
        actor: actor.as_str().to_owned(),
        identity_id: identity_id.clone(),
        identity_name,
        session_id,
        remote_token: request.remote.as_ref().map(|(id, _)| id.clone()),
        remote_token_name: request
            .remote
            .as_ref()
            .map(|(_, name)| name.clone())
            .or(remote_name),
        created_at: now,
        operation_id: None,
        operation_collected: false,
        recorded_at: None,
        instruction,
        updated_at: now,
    };
    let event = CommentEventRow {
        repo_id: repo,
        id: crate::identity::new_id("cev_"),
        comment_id: id.clone(),
        change_id: change.clone(),
        event: event_name.to_owned(),
        from_state: None,
        to_state,
        cause: "request".to_owned(),
        agent,
        actor: actor.as_str().to_owned(),
        identity_id,
        remote_token: request.remote.as_ref().map(|(id, _)| id.clone()),
        created_at: now,
        ..CommentEventRow::blank(repo)
    };
    let mut meta = OpMeta::new(
        OpKind::Comment,
        jj_comments::comment_description(event_name, &id, &change),
    );
    meta.workspace = request.workspace.as_ref().map(|record| record.id.as_hex());
    meta.agent = request.agent.clone().or_else(|| {
        request
            .workspace
            .as_ref()
            .map(|record| record.owner.clone())
    });
    let (op_id, _) = record(
        state,
        repo,
        vec![Planned {
            event,
            comment: Some(comment),
            kind: kind.as_str(),
            round,
        }],
        meta,
    )
    .await?;
    let comment = state
        .db
        .comment(repo, &id)?
        .ok_or_else(|| CommentError::NotFound { id: id.clone() })?;
    tracing::info!(comment = %id, change_id = %change, kind = kind.as_str(), op_id = %op_id, "comment recorded");
    Ok(CreateOutcome {
        comment,
        op_id,
        confirm_refused,
    })
}

/// A client's move of an instruction.
#[derive(Clone, Debug)]
pub struct UpdateRequest {
    /// The repository.
    pub repo: RepoId,
    /// The comment.
    pub id: String,
    /// The target; `None` is refused.
    pub to: Option<State>,
    /// The state the caller expects.
    pub expected: Option<State>,
    /// What the session said.
    pub summary: String,
    /// The workspace the caller works in.
    pub workspace: Option<WorkspaceRecord>,
    /// The call's agent, marked with its remote token.
    pub agent: Option<String>,
    /// The remote token, `(id, name)`.
    pub remote: Option<(String, String)>,
}

/// What [`update`] or [`claim`] did.
#[derive(Debug, Default)]
pub struct MoveOutcome {
    /// The comment afterwards; `None` for a claim that found nothing.
    pub comment: Option<CommentRow>,
    /// The operation; `None` when nothing changed.
    pub op_id: Option<OperationIdHex>,
}

/// Moves an instruction for a client.
pub async fn update(
    state: &Arc<AppState>,
    request: UpdateRequest,
) -> Result<MoveOutcome, CommentError> {
    let state = Arc::clone(state);
    let repo = request.repo;
    let span = tracing::Span::current();
    tokio::spawn(async move { update_inner(&state, request).await }.instrument(span))
        .await
        .map_err(|_| CommentError::Thread(RepoThreadError::Panicked { id: repo }))?
}

#[expect(
    clippy::too_many_lines,
    reason = "the checks in their order, then the event it records"
)]
async fn update_inner(
    state: &AppState,
    request: UpdateRequest,
) -> Result<MoveOutcome, CommentError> {
    let to = request.to.ok_or(CommentError::StateRequired)?;
    if matches!(to, State::AwaitingConfirmation | State::Delivered) {
        return Err(CommentError::StateRequired);
    }
    if !valid_text(&request.summary, MAX_SUMMARY_BYTES, false) {
        return Err(CommentError::InvalidText);
    }
    let repo = request.repo;
    let create_lock = state.manager.repo_lock(repo);
    let _shared = create_lock.read().await;
    let comment_lock = state.comments.lock(repo);
    let _held = comment_lock.lock().await;
    let id = request.id.trim().to_owned();
    let comment = state
        .db
        .comment(repo, &id)?
        .ok_or_else(|| CommentError::NotFound { id: id.clone() })?;
    let Some(columns) = &comment.instruction else {
        return Err(CommentError::NotAnInstruction { id });
    };
    let from = State::parse(&columns.state).unwrap_or(State::Cancelled);
    // Already there: a retry (a confirmation or release sent again) does
    // nothing, whatever state it expected to move from.
    if from == to {
        return Ok(MoveOutcome {
            comment: Some(comment),
            op_id: None,
        });
    }
    if let Some(expected) = request.expected
        && expected != from
    {
        return Err(CommentError::ExpectedState {
            expected,
            state: from,
        });
    }
    if !allowed(from, to) {
        return Err(CommentError::NotAllowed { state: from, to });
    }
    // PENDING is both a confirmation (from awaiting) and a release (from
    // delivered): a release must say so, so a confirmation sent twice, or
    // after the instruction was taken, never hands it out again.
    if from == State::Delivered
        && to == State::Pending
        && request.expected != Some(State::Delivered)
    {
        return Err(CommentError::NotAllowed { state: from, to });
    }
    if let Some((token, _)) = &request.remote {
        // The writer confirms and cancels; the claimer moves it after.
        let owner = if matches!(event_of(from, to), "confirm" | "cancel") {
            comment.remote_token.as_deref()
        } else {
            columns.claimed_token.as_deref()
        };
        if owner != Some(token.as_str()) {
            return Err(CommentError::RemoteNotOwned { id });
        }
    }
    let (agent, _) = agent_parts(request.agent.as_deref());
    let actor = if request.remote.is_some() || agent.is_some() {
        Actor::Agent
    } else {
        Actor::Human
    };
    let (identity_id, _, _) =
        crate::lifecycle::identity_for(state, request.workspace.as_ref(), request.remote.as_ref());
    let event_name = event_of(from, to);
    let event = CommentEventRow {
        repo_id: repo,
        id: crate::identity::new_id("cev_"),
        comment_id: id.clone(),
        change_id: comment.change_id.clone(),
        event: event_name.to_owned(),
        from_state: Some(from.as_str().to_owned()),
        to_state: Some(to.as_str().to_owned()),
        cause: "request".to_owned(),
        agent,
        actor: actor.as_str().to_owned(),
        identity_id,
        remote_token: request.remote.as_ref().map(|(id, _)| id.clone()),
        summary: Some(request.summary.clone()).filter(|summary| !summary.is_empty()),
        created_at: Timestamp::now(),
        ..CommentEventRow::blank(repo)
    };
    let mut meta = OpMeta::new(
        OpKind::Comment,
        jj_comments::comment_description(event_name, &id, &comment.change_id),
    );
    meta.workspace = request.workspace.as_ref().map(|record| record.id.as_hex());
    meta.agent = request.agent.clone().or_else(|| {
        request
            .workspace
            .as_ref()
            .map(|record| record.owner.clone())
    });
    let (op_id, _) = record(
        state,
        repo,
        vec![Planned {
            event,
            comment: None,
            kind: "instruction",
            round: comment.round,
        }],
        meta,
    )
    .await?;
    Ok(MoveOutcome {
        comment: state.db.comment(repo, &id)?,
        op_id: Some(op_id),
    })
}

/// A client's claim of an instruction for a session.
#[derive(Clone, Debug)]
pub struct ClaimRequest {
    /// The repository.
    pub repo: RepoId,
    /// The workspace the session works in.
    pub workspace: Option<WorkspaceRecord>,
    /// The user email revsets' `mine()` reads.
    pub user_email: String,
    /// The repository's root.
    pub root: std::path::PathBuf,
    /// The session's client session; empty for none.
    pub client_session: String,
    /// A revision naming the change; empty for none.
    pub change: String,
    /// One instruction; empty for the oldest.
    pub comment_id: String,
    /// Take another session's instruction on the change.
    pub take: bool,
    /// Start it.
    pub start: bool,
    /// `cli`, `hook`, `mcp` or `sdk`.
    pub channel: &'static str,
    /// The call's agent, marked with its remote token.
    pub agent: Option<String>,
    /// The remote token, `(id, name)`.
    pub remote: Option<(String, String)>,
}

/// Takes an instruction for a session.
pub async fn claim(
    state: &Arc<AppState>,
    request: ClaimRequest,
) -> Result<MoveOutcome, CommentError> {
    let state = Arc::clone(state);
    let repo = request.repo;
    let span = tracing::Span::current();
    tokio::spawn(async move { claim_inner(&state, request).await }.instrument(span))
        .await
        .map_err(|_| CommentError::Thread(RepoThreadError::Panicked { id: repo }))?
}

#[expect(
    clippy::too_many_lines,
    reason = "the scope, the match, the remote rule, then the record"
)]
async fn claim_inner(state: &AppState, request: ClaimRequest) -> Result<MoveOutcome, CommentError> {
    let client_session = request.client_session.trim().to_owned();
    if client_session.is_empty() && request.workspace.is_none() && request.change.trim().is_empty()
    {
        return Err(CommentError::ClaimScopeRequired);
    }
    if let Some((token, _)) = &request.remote
        && request.take
        && !token_linked(state, token)
    {
        return Err(CommentError::RemoteIdentityRequired);
    }
    let repo = request.repo;
    let handle = state.manager.handle(repo)?;
    let create_lock = state.manager.repo_lock(repo);
    let _shared = create_lock.read().await;
    let comment_lock = state.comments.lock(repo);
    let _held = comment_lock.lock().await;
    let comment_id = request.comment_id.trim().to_owned();
    if !comment_id.is_empty() {
        let found = state
            .db
            .comment(repo, &comment_id)?
            .ok_or_else(|| CommentError::NotFound {
                id: comment_id.clone(),
            })?;
        if found.instruction.is_none() {
            return Err(CommentError::NotAnInstruction { id: comment_id });
        }
    }
    let change = if request.change.trim().is_empty() {
        request
            .workspace
            .as_ref()
            .map(|record| record.change_id.clone())
            .filter(|change| !change.is_empty())
    } else {
        let request = request.clone();
        let resolved = handle
            .call_async(move |head| {
                async move {
                    let workspace = request.workspace.as_ref().map(|record| record.id);
                    crate::workspace::refresh_for_resolve(
                        head,
                        &crate::workspace::import_meta(workspace, request.agent.as_deref()),
                    )
                    .await?;
                    let repo = head.repo.readonly();
                    let jj_name = workspace.map(|id| id.jj_workspace_name());
                    let scope = RevsetScope {
                        workspace: jj_name.as_deref(),
                        user_email: &request.user_email,
                        root: &request.root,
                    };
                    let resolved = revset::resolve_revision(&repo, &request.change, &scope).await?;
                    let describer = Describer::new(&repo, &scope)?;
                    let info = describer.describe(&resolved.commit_id).await?;
                    Ok::<_, CommentError>(info.change_id.to_string())
                }
                .boxed_local()
            })
            .await??;
        Some(resolved)
    };
    let mut sessions: Vec<String> = Vec::new();
    if !client_session.is_empty() {
        sessions.push(client_session.clone());
    }
    let workspace_session = match &request.workspace {
        Some(record) => state.db.live_provenance_session(&record.id.as_hex())?,
        None => None,
    };
    if let Some(declared) = workspace_session
        .as_ref()
        .and_then(|session| session.declared.client_session.clone())
        && !sessions.contains(&declared)
    {
        sessions.push(declared);
    }
    let query = ClaimQuery {
        comment_id: Some(comment_id).filter(|id| !id.is_empty()),
        client_sessions: sessions.clone(),
        workspace_id: request.workspace.as_ref().map(|record| record.id.as_hex()),
        change_id: change.clone(),
        take: request.take,
    };
    let Some(found) = state.db.claimable_instruction(repo, &query)? else {
        return Ok(MoveOutcome::default());
    };
    let Some(columns) = &found.instruction else {
        return Ok(MoveOutcome::default());
    };
    let own = columns
        .target_client_session
        .as_ref()
        .is_some_and(|target| sessions.contains(target))
        || (columns.target_client_session.is_none()
            && columns.target_workspace_id.is_some()
            && columns.target_workspace_id == query.workspace_id)
        || columns.target_source == "none";
    if let Some((token, _)) = &request.remote
        && !request.take
    {
        let proposer = state
            .db
            .lifecycle_row(repo, &found.change_id)?
            .and_then(|row| row.proposed_by_token);
        if proposer.as_deref() != Some(token.as_str()) {
            return Err(CommentError::RemoteNotOwned {
                id: found.id.clone(),
            });
        }
    }
    let to = if request.start {
        State::Running
    } else {
        State::Delivered
    };
    let (agent, _) = agent_parts(request.agent.as_deref());
    let (identity_id, _, _) =
        crate::lifecycle::identity_for(state, request.workspace.as_ref(), request.remote.as_ref());
    let claimed_session = Some(client_session.clone())
        .filter(|session| !session.is_empty())
        .or_else(|| {
            workspace_session
                .as_ref()
                .and_then(|session| session.declared.client_session.clone())
        });
    let event = CommentEventRow {
        repo_id: repo,
        id: crate::identity::new_id("cev_"),
        comment_id: found.id.clone(),
        change_id: found.change_id.clone(),
        event: "claim".to_owned(),
        from_state: Some(State::Pending.as_str().to_owned()),
        to_state: Some(to.as_str().to_owned()),
        cause: "request".to_owned(),
        agent: agent.clone(),
        actor: Actor::Agent.as_str().to_owned(),
        identity_id,
        remote_token: request.remote.as_ref().map(|(id, _)| id.clone()),
        claimed_client_session: claimed_session,
        claimed_workspace_id: request.workspace.as_ref().map(|record| record.id.as_hex()),
        claimed_agent: agent.or_else(|| {
            request
                .workspace
                .as_ref()
                .map(|record| record.owner.clone())
        }),
        claimed_token: request.remote.as_ref().map(|(id, _)| id.clone()),
        channel: Some(request.channel.to_owned()),
        taken: !own,
        created_at: Timestamp::now(),
        ..CommentEventRow::blank(repo)
    };
    let mut meta = OpMeta::new(
        OpKind::Comment,
        jj_comments::comment_description("claim", &found.id, &found.change_id),
    );
    meta.workspace = request.workspace.as_ref().map(|record| record.id.as_hex());
    meta.agent = request.agent.clone().or_else(|| {
        request
            .workspace
            .as_ref()
            .map(|record| record.owner.clone())
    });
    let (op_id, _) = record(
        state,
        repo,
        vec![Planned {
            event,
            comment: None,
            kind: "instruction",
            round: found.round,
        }],
        meta,
    )
    .await?;
    tracing::info!(comment = %found.id, taken = !own, op_id = %op_id, "instruction claimed");
    Ok(MoveOutcome {
        comment: state.db.comment(repo, &found.id)?,
        op_id: Some(op_id),
    })
}

/// A daemon move of `comment` from `from` to `to`, with `cause`.
fn daemon_event(
    comment: &CommentRow,
    from: State,
    to: State,
    cause: &str,
    transition: Option<&str>,
) -> Planned {
    Planned {
        event: CommentEventRow {
            repo_id: comment.repo_id,
            id: crate::identity::new_id("cev_"),
            comment_id: comment.id.clone(),
            change_id: comment.change_id.clone(),
            event: event_of(from, to).to_owned(),
            from_state: Some(from.as_str().to_owned()),
            to_state: Some(to.as_str().to_owned()),
            cause: cause.to_owned(),
            transition_id: transition.map(str::to_owned),
            actor: Actor::Daemon.as_str().to_owned(),
            created_at: Timestamp::now(),
            ..CommentEventRow::blank(comment.repo_id)
        },
        comment: None,
        kind: "instruction",
        round: comment.round,
    }
}

/// Records `planned`, daemon moves, in one operation under `repo`'s locks.
async fn record_daemon(
    state: &AppState,
    repo: RepoId,
    planned: Vec<Planned>,
) -> Result<Option<OperationIdHex>, CommentError> {
    let Some(first) = planned.first() else {
        return Ok(None);
    };
    let meta = OpMeta::new(
        OpKind::Comment,
        jj_comments::comment_description(
            &first.event.event,
            &first.event.comment_id,
            &first.event.change_id,
        ),
    );
    let (op_id, _) = record(state, repo, planned, meta).await?;
    Ok(Some(op_id))
}

/// Finishes or cancels the instructions committed transitions make moot:
/// a request to propose finishes the change's instructions delivered
/// before it (DONE, `reproposed`); a landing or a supersede cancels its
/// unconfirmed and pending ones (`change_final`). One operation for the
/// lot.
pub async fn after_transitions(
    state: &AppState,
    repo: RepoId,
    rows: &[TransitionRow],
) -> Result<Option<OperationIdHex>, CommentError> {
    let create_lock = state.manager.repo_lock(repo);
    let _shared = create_lock.read().await;
    let comment_lock = state.comments.lock(repo);
    let _held = comment_lock.lock().await;
    let mut planned = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for row in rows {
        let Some(recorded_at) = row.recorded_at else {
            continue;
        };
        if row.to_state == "proposed" && row.cause == "request" {
            for comment in state.db.instructions_of_change(
                repo,
                &row.change_id,
                &[State::Delivered.as_str(), State::Running.as_str()],
            )? {
                let delivered_before = comment
                    .instruction
                    .as_ref()
                    .and_then(|columns| columns.delivered_at)
                    .is_some_and(|at| at <= recorded_at);
                if delivered_before && seen.insert(comment.id.clone()) {
                    let from = comment
                        .instruction
                        .as_ref()
                        .and_then(|columns| State::parse(&columns.state))
                        .unwrap_or(State::Delivered);
                    planned.push(daemon_event(
                        &comment,
                        from,
                        State::Done,
                        "reproposed",
                        Some(&row.id),
                    ));
                }
            }
        } else if row.to_state == "landed" || row.to_state == "superseded" {
            for comment in state.db.instructions_of_change(
                repo,
                &row.change_id,
                &[
                    State::AwaitingConfirmation.as_str(),
                    State::Pending.as_str(),
                ],
            )? {
                if seen.insert(comment.id.clone()) {
                    let from = comment
                        .instruction
                        .as_ref()
                        .and_then(|columns| State::parse(&columns.state))
                        .unwrap_or(State::Pending);
                    planned.push(daemon_event(
                        &comment,
                        from,
                        State::Cancelled,
                        "change_final",
                        Some(&row.id),
                    ));
                }
            }
        }
    }
    record_daemon(state, repo, planned).await
}

/// Finishes or cancels, from the database alone, every instruction a
/// committed transition made moot whose notice never reached the sweep (a
/// restart dropped the queue, or start-up committed the transition late):
/// the same moves [`after_transitions`] makes. Returns how many it moved.
pub async fn reconcile(state: &AppState) -> Result<usize, CommentError> {
    let mut moved = 0;
    for repo in state.db.repos_with_open_instructions()? {
        if state.repos.get(repo).is_none() {
            continue;
        }
        let create_lock = state.manager.repo_lock(repo);
        let _shared = create_lock.read().await;
        let comment_lock = state.comments.lock(repo);
        let _held = comment_lock.lock().await;
        let mut planned = Vec::new();
        for (comment, cause, transition) in state.db.moot_instructions(repo)? {
            let Some(from) = comment
                .instruction
                .as_ref()
                .and_then(|columns| State::parse(&columns.state))
            else {
                continue;
            };
            let to = if cause == "reproposed" {
                State::Done
            } else {
                State::Cancelled
            };
            planned.push(daemon_event(&comment, from, to, cause, Some(&transition)));
        }
        moved += planned.len();
        if let Err(error) = record_daemon(state, repo, planned).await {
            tracing::warn!(%repo, error = %ErrorChain(&error), "cannot reconcile instructions");
        }
    }
    Ok(moved)
}

/// Ends every unfinished instruction untouched for [`EXPIRY`] as of `now`:
/// CANCELLED before it runs, FAILED while running, cause `expired`; one
/// operation per repository. Returns how many it ended.
pub async fn expire(state: &AppState, now: Timestamp) -> Result<usize, CommentError> {
    let cutoff = Timestamp(
        now.0
            .saturating_sub(i64::try_from(EXPIRY.as_millis()).unwrap_or(0)),
    );
    let untouched = state.db.stale_instructions(cutoff, EXPIRE_BATCH)?;
    let mut by_repo: HashMap<RepoId, Vec<CommentRow>> = HashMap::new();
    for comment in untouched {
        by_repo.entry(comment.repo_id).or_default().push(comment);
    }
    let mut ended = 0;
    for (repo, comments) in by_repo {
        if state.repos.get(repo).is_none() {
            continue;
        }
        let create_lock = state.manager.repo_lock(repo);
        let _shared = create_lock.read().await;
        let comment_lock = state.comments.lock(repo);
        let _held = comment_lock.lock().await;
        let mut planned = Vec::new();
        for comment in comments {
            // Read again under the lock: a move since the query wins.
            let Some(current) = state.db.comment(repo, &comment.id)? else {
                continue;
            };
            let Some(from) = current
                .instruction
                .as_ref()
                .and_then(|columns| State::parse(&columns.state))
            else {
                continue;
            };
            if from.is_final() || current.updated_at >= cutoff {
                continue;
            }
            let to = if from == State::Running {
                State::Failed
            } else {
                State::Cancelled
            };
            planned.push(daemon_event(&current, from, to, "expired", None));
        }
        ended += planned.len();
        if let Err(error) = record_daemon(state, repo, planned).await {
            tracing::warn!(%repo, error = %ErrorChain(&error), "cannot expire instructions");
        }
    }
    Ok(ended)
}

/// Starts the sweep: it acts on the transitions the lifecycle hands it and
/// expires instructions every ten minutes; it stops at shutdown.
pub(crate) fn start(state: &Arc<AppState>) {
    let Some(mut receiver) = state
        .comments
        .receiver
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
    else {
        return;
    };
    let weak = Arc::downgrade(state);
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(SWEEP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = shutdown.cancelled() => return,
                next = receiver.recv() => {
                    let Some((repo, rows)) = next else { return };
                    let Some(state) = weak.upgrade() else { return };
                    if let Err(error) = after_transitions(&state, repo, &rows).await {
                        tracing::warn!(%repo, error = %ErrorChain(&error), "cannot finish the instructions a transition made moot");
                    }
                }
                _ = tick.tick() => {
                    let Some(state) = weak.upgrade() else { return };
                    // The first tick is at start: what a restart dropped.
                    if let Err(error) = reconcile(&state).await {
                        tracing::warn!(error = %ErrorChain(&error), "cannot reconcile instructions");
                    }
                    if let Err(error) = expire(&state, Timestamp::now()).await {
                        tracing::warn!(error = %ErrorChain(&error), "cannot expire instructions");
                    }
                }
            }
        }
    });
}

/// At start, before the socket serves: commits every pending comment event
/// whose operation exists, and drops the others with their pending
/// comments. A repository that is not open keeps its pending rows.
pub(crate) async fn resolve_pending(state: &AppState) {
    let pending = match state.db.pending_comment_events() {
        Ok(pending) => pending,
        Err(error) => {
            tracing::error!(error = %ErrorChain(&error), "cannot read pending comment events");
            return;
        }
    };
    let mut by_repo: HashMap<RepoId, Vec<CommentEventRow>> = HashMap::new();
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
                    Ok::<_, CommentError>(
                        jj_comments::find_comment_events(&repo, &ids, not_before).await?,
                    )
                }
                .boxed_local()
            })
            .await;
        let found = match found {
            Ok(Ok(found)) => found,
            Ok(Err(error)) => {
                tracing::warn!(%repo, error = %ErrorChain(&error), "cannot look for pending comment events' operations");
                continue;
            }
            Err(error) => {
                tracing::warn!(%repo, error = %ErrorChain(&error), "cannot look for pending comment events' operations");
                continue;
            }
        };
        let (mut committed, mut dropped) = (0, 0);
        for row in rows {
            let outcome = match found.get(&row.id) {
                Some(op) => state
                    .db
                    .commit_comment_event(repo, &row.id, op.op_id.as_str(), Timestamp::from(op.end))
                    .map(|_| committed += 1),
                None => state
                    .db
                    .delete_pending_comment_event(repo, &row.id)
                    .map(|()| dropped += 1),
            };
            if let Err(error) = outcome {
                tracing::warn!(%repo, event = %row.id, error = %ErrorChain(&error), "cannot resolve a pending comment event");
            }
        }
        tracing::info!(%repo, committed, dropped, "resolved pending comment events");
    }
}

/// GC's step after its jj step: maps the operation ids of `repo`'s
/// comments and events through the rewrite, marking those it dropped.
pub(crate) fn map_operations(
    state: &AppState,
    repo: RepoId,
    rewritten: &[(OperationIdHex, OperationIdHex)],
) {
    let map: HashMap<String, String> = rewritten
        .iter()
        .map(|(old, new)| (old.to_string(), new.to_string()))
        .collect();
    match state.db.map_comment_operations(repo, &map) {
        Ok((mapped, collected)) if mapped + collected > 0 => {
            tracing::info!(%repo, mapped, collected, "mapped comments' operation ids");
        }
        Ok(_) => {}
        Err(error) => {
            tracing::warn!(%repo, error = %ErrorChain(&error), "cannot map comments' operation ids");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table() {
        use State as S;
        let allowed_pairs = [
            (S::AwaitingConfirmation, S::Pending),
            (S::AwaitingConfirmation, S::Cancelled),
            (S::Pending, S::Cancelled),
            (S::Delivered, S::Pending),
            (S::Delivered, S::Running),
            (S::Delivered, S::Done),
            (S::Delivered, S::Failed),
            (S::Delivered, S::Cancelled),
            (S::Running, S::Done),
            (S::Running, S::Failed),
        ];
        for from in State::ALL {
            for to in State::ALL {
                assert_eq!(
                    allowed(from, to),
                    allowed_pairs.contains(&(from, to)),
                    "{from} -> {to}"
                );
            }
            if from.is_final() {
                assert!(State::ALL.into_iter().all(|to| !allowed(from, to)));
            }
        }
        assert_eq!(event_of(S::AwaitingConfirmation, S::Pending), "confirm");
        assert_eq!(event_of(S::Delivered, S::Pending), "release");
        assert_eq!(event_of(S::Pending, S::Running), "claim");
        assert_eq!(event_of(S::Delivered, S::Running), "start");
        assert_eq!(event_of(S::Running, S::Failed), "finish");
        assert_eq!(event_of(S::Pending, S::Cancelled), "cancel");
        for state in State::ALL {
            assert_eq!(State::parse(state.as_str()), Some(state));
        }
    }

    #[test]
    fn texts() {
        assert!(valid_text("fix it\n\tplease", MAX_TEXT_BYTES, true));
        assert!(!valid_text("  ", MAX_TEXT_BYTES, true));
        assert!(valid_text("", MAX_SUMMARY_BYTES, false));
        assert!(!valid_text("\u{1b}[2J", MAX_TEXT_BYTES, true));
        assert!(!valid_text(
            &"a".repeat(MAX_TEXT_BYTES + 1),
            MAX_TEXT_BYTES,
            true
        ));
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn ceilings() {
        let known = Estimate {
            checks_known: true,
            check_wall_ms: 1000,
            agent_known: true,
            agent_wall_ms: 5000,
            ..Estimate::default()
        };
        let within = Ceilings {
            max_check_wall_ms: 1000,
            max_agent_wall_ms: 5000,
            allow_unknown: false,
        };
        assert_eq!(ceiling_refusal(&known, &within), None);
        let tight = Ceilings {
            max_check_wall_ms: 999,
            ..within
        };
        assert!(ceiling_refusal(&known, &tight).is_some());
        let unknown = Estimate::default();
        assert!(ceiling_refusal(&unknown, &within).is_some());
        let lenient = Ceilings {
            allow_unknown: true,
            ..within
        };
        assert_eq!(ceiling_refusal(&unknown, &lenient), None);
        assert_eq!(ceiling_refusal(&unknown, &Ceilings::default()), None);
    }

    #[test]
    fn the_digest_ignores_the_time() {
        let mut a = Estimate {
            checks: 2,
            ..Estimate::default()
        };
        let mut b = a.clone();
        a.seal(Timestamp(1));
        b.seal(Timestamp(2));
        assert_eq!(a.digest, b.digest);
        assert_ne!(a.computed_at, b.computed_at);
    }
}
