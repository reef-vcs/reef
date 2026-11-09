//! Resolver assignments and the resolver queue (R3.27, ADR 0068;
//! `docs/conflicts.md`, "Who resolves a conflict").
//!
//! Conflicts are jj's and derived on every read (R3.25); an assignment is
//! the one thing about them only Reef knows: who should resolve each
//! conflict a change introduces. It is keyed by the change and the
//! conflict's kind, at most one open at a time, so the resolver's own
//! progress (a partial resolution changes the conflict id) never closes it.
//!
//! - **Owners** ([`default_owner`]): the newest live provenance session
//!   that recorded the conflicted change (the later change: the one moved
//!   onto the other), else its newest ended session, else the live session
//!   of the one active workspace editing it; otherwise nobody, `unowned`.
//! - **Liveness**: an owner's session has ended when its workspace has no
//!   live provenance session (released or reclaimed). The assignment then
//!   follows a live session of the same identity and declared client
//!   session, or waits [`OWNER_GRACE`] and is queued; a queued one whose
//!   owner ended returns to that client session if it comes back before
//!   anyone claims it.
//! - **The queue** is the `queued` rows, by priority then time queued.
//!   Resolvers claim from it for a workspace's session ([`claim`]), which
//!   holds the claim while it lives; owners release ([`release`]); a
//!   caller assigns, queues, reprioritizes, or takes one over ([`assign`]).
//!
//! The follower ([`follow`]) keeps the rows in step with the operation log;
//! every write looks first ([`look`]) under the repository's assignment
//! lock. Every change of a row is an event, written with it, and announced
//! afterwards by an operation that changes nothing in the view
//! (`reef.conflict`): the rows are the truth, the operation the notice.

pub mod follow;
pub mod messages;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::FutureExt as _;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::assignment::{self as jj_assignment, AssignmentNotice, Conflicted, Visible};
use reef_lib::jj::conflict::ConflictError;
use reef_lib::jj::oplog::{self, OpMeta};
use reef_lib::jj::revset::{self, RevisionError, RevsetScope};
use reef_lib::jj::{ChangeIdHex, OpKind, OperationIdHex};

use crate::daemon::AppState;
use crate::db::{
    AssignmentEventRow, AssignmentRow, DbError, OwnerColumns, ProvenanceFilter,
    ProvenanceSessionRow,
};
use crate::error::{Advice, Advise};
use crate::model::{RepoId, Timestamp, WorkspaceId, WorkspaceRecord, WorkspaceState};

/// The kind of jj's stored conflicts, as rows name it.
pub const KIND_TREE: &str = "tree";
/// How long an ended owner keeps its assignment before it is queued, unless
/// it is followed first.
pub const OWNER_GRACE: Duration = Duration::from_mins(5);
/// How long after it closed an assignment reopens when the same conflict
/// comes back.
pub const REOPEN_WINDOW: Duration = Duration::from_hours(24);
/// How long GC keeps a closed assignment and its events.
pub const RETENTION: Duration = Duration::from_hours(24 * 30);
/// Open assignments per repository, at most (S-153).
pub const MAX_OPEN: usize = 1000;
/// Events per assignment past which claims, releases and assignments are
/// refused (S-153).
pub const MAX_EVENTS: u32 = 1000;
/// The lowest and highest priority.
pub const MIN_PRIORITY: i32 = -100;
/// The highest priority.
pub const MAX_PRIORITY: i32 = 100;
/// The conflicted commits one look reads, at most.
const SCAN_CAP: usize = 2 * MAX_OPEN;
/// The shortest time between two `changed` events of one assignment.
const CHANGED_EVERY: Duration = Duration::from_mins(1);
/// Provenance records read to find a change's sessions.
const RECORDS_READ: usize = 200;
/// The revset the follower reads, as `ListConflicts` does by default.
const CONFLICTS: &str = "mutable() & conflicts()";

/// Where an assignment stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum State {
    /// A live session owns it.
    Assigned,
    /// In the resolver queue.
    Queued,
    /// A person took it over.
    Taken,
    /// Nobody owns it, and nobody queued it.
    Unowned,
    /// The change no longer carries the conflict.
    Closed,
}

impl State {
    /// The column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Assigned => "assigned",
            State::Queued => "queued",
            State::Taken => "taken",
            State::Unowned => "unowned",
            State::Closed => "closed",
        }
    }

    /// The state a column's text names.
    pub fn parse(text: &str) -> Option<State> {
        [
            State::Assigned,
            State::Queued,
            State::Taken,
            State::Unowned,
            State::Closed,
        ]
        .into_iter()
        .find(|state| state.as_str() == text)
    }
}

/// Why an assignment's state and owner are what they are.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Reason {
    /// The change's originating agent.
    Default,
    /// Followed to a live session of the same client session.
    Followed,
    /// Back to its ended owner's client session.
    Returned,
    /// Queued after its owner's session ended.
    OwnerEnded,
    /// Nobody to own it.
    NoOwner,
    /// A resolver claimed it.
    Claimed,
    /// Its owner gave it up.
    Released,
    /// A caller assigned, queued or reprioritized it.
    Assigned,
    /// A person took it over.
    Taken,
    /// Its conflict changed.
    Changed,
    /// Closed: the change carries no conflict.
    Resolved,
    /// Closed: the change only inherits it.
    Inherited,
    /// Closed: the change has no visible commit.
    Abandoned,
    /// Closed: the change is not mutable.
    Immutable,
    /// Opened again within [`REOPEN_WINDOW`].
    Reopened,
}

impl Reason {
    /// Every reason.
    pub const ALL: [Reason; 15] = [
        Reason::Default,
        Reason::Followed,
        Reason::Returned,
        Reason::OwnerEnded,
        Reason::NoOwner,
        Reason::Claimed,
        Reason::Released,
        Reason::Assigned,
        Reason::Taken,
        Reason::Changed,
        Reason::Resolved,
        Reason::Inherited,
        Reason::Abandoned,
        Reason::Immutable,
        Reason::Reopened,
    ];

    /// The column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Default => "default",
            Reason::Followed => "followed",
            Reason::Returned => "returned",
            Reason::OwnerEnded => "owner_ended",
            Reason::NoOwner => "no_owner",
            Reason::Claimed => "claimed",
            Reason::Released => "released",
            Reason::Assigned => "assigned",
            Reason::Taken => "taken",
            Reason::Changed => "changed",
            Reason::Resolved => "resolved",
            Reason::Inherited => "inherited",
            Reason::Abandoned => "abandoned",
            Reason::Immutable => "immutable",
            Reason::Reopened => "reopened",
        }
    }

    /// The reason a column's text names.
    pub fn parse(text: &str) -> Option<Reason> {
        Reason::ALL
            .into_iter()
            .find(|reason| reason.as_str() == text)
    }
}

/// Who asked for a change of an assignment, as its event records it.
#[derive(Clone, Debug, Default)]
pub struct Caller {
    /// `human`, `agent` or `daemon`.
    pub actor: String,
    /// The call's agent, without its remote token.
    pub agent: Option<String>,
    /// The call's agent as operations carry it (marked with its remote
    /// token, `RemoteToken::mark`).
    pub marked_agent: Option<String>,
    /// The caller's identity, as its workspace's session binds it.
    pub identity_id: Option<String>,
    /// The person identity a takeover is in the name of: the remote
    /// token's linked one, else the daemon's default, whatever workspace
    /// the call names, so a take and its release agree.
    pub person_identity_id: Option<String>,
    /// The caller's workspace's provenance session.
    pub session_id: Option<String>,
    /// The remote token, `(id, name)`.
    pub remote: Option<(String, String)>,
    /// The caller's workspace, in hex.
    pub workspace: Option<String>,
}

impl Caller {
    /// The daemon itself, for what the follower observes.
    pub fn daemon() -> Caller {
        Caller {
            actor: "daemon".to_owned(),
            ..Caller::default()
        }
    }
}

/// Which assignment a request names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Named {
    /// By its id.
    Id(String),
    /// A change's open assignment, else its newest.
    Change(String),
    /// The queue's head (claims only).
    QueueHead,
}

/// Why an assignment request was refused, or failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AssignError {
    /// No such assignment.
    #[error("no resolver assignment {assignment}")]
    NotFound {
        /// What was asked for.
        assignment: String,
    },
    /// Nothing is queued.
    #[error("the resolver queue is empty")]
    QueueEmpty,
    /// A release named no assignment, and the workspace owns none, or
    /// several.
    #[error("say which assignment: the workspace owns {}", owned_text(owned))]
    WhichAssignment {
        /// The assignments the workspace owns.
        owned: Vec<String>,
    },
    /// Someone else holds it.
    #[error(
        "assignment {assignment} is {state}{}",
        owner_text(owner_workspace, owner_agent)
    )]
    Held {
        /// The assignment.
        assignment: String,
        /// Its state.
        state: String,
        /// Its owner's workspace.
        owner_workspace: String,
        /// Its owner's agent.
        owner_agent: String,
    },
    /// Nobody owns it, and nobody queued it.
    #[error("assignment {assignment} is unowned: Reef never recorded work on its change")]
    Unowned {
        /// The assignment.
        assignment: String,
    },
    /// It closed.
    #[error("assignment {assignment} is closed")]
    Closed {
        /// The assignment.
        assignment: String,
    },
    /// The workspace has no live provenance session.
    #[error("workspace {workspace} is not active, or has no live provenance session")]
    NoLiveSession {
        /// The workspace.
        workspace: String,
    },
    /// An agent asked to take a conflict over.
    #[error("only a person takes a conflict over; an agent claims it")]
    TakeNeedsPerson,
    /// A remote caller asked to take a conflict over.
    #[error("a conflict is taken over on the daemon's machine, not through the remote listener")]
    RemoteHuman,
    /// The conflict is not the one the caller read.
    #[error("the conflict changed: it is {conflict_id} now")]
    ConflictChanged {
        /// The conflict's id now.
        conflict_id: String,
    },
    /// The assignment holds [`MAX_EVENTS`] events.
    #[error("assignment {assignment} has {MAX_EVENTS} events, the most it records")]
    EventLimit {
        /// The assignment.
        assignment: String,
    },
    /// A priority out of range.
    #[error("priority {priority} is outside {MIN_PRIORITY} to {MAX_PRIORITY}")]
    InvalidPriority {
        /// The priority given.
        priority: i32,
    },
    /// A reason over 4 KiB or with a control character.
    #[error("a reason must be at most 4 KiB, with no control characters but newlines and tabs")]
    InvalidReason,
    /// Neither a target nor a priority.
    #[error(
        "say where the assignment goes (a workspace, the queue, or take it), or give a priority"
    )]
    TargetRequired,
    /// A revision did not name one change.
    #[error(transparent)]
    Revision(#[from] RevisionError),
    /// The database failed.
    #[error("the daemon's database failed")]
    Db(#[from] DbError),
    /// jj could not be read.
    #[error("could not read the repository's conflicts")]
    Read(#[source] Box<dyn std::error::Error + Send + Sync>),
}

fn owned_text(owned: &[String]) -> String {
    if owned.is_empty() {
        "none".to_owned()
    } else {
        owned.join(", ")
    }
}

fn owner_text(workspace: &str, agent: &str) -> String {
    match (workspace.is_empty(), agent.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!(" (workspace {workspace})"),
        (true, false) => format!(" ({agent})"),
        (false, false) => format!(" ({agent}, workspace {workspace})"),
    }
}

impl From<ConflictError> for AssignError {
    fn from(error: ConflictError) -> Self {
        AssignError::Read(Box::new(error))
    }
}

impl Classify for AssignError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotFound { .. } | Self::QueueEmpty => ErrorKind::NotFound,
            Self::Held { .. }
            | Self::Unowned { .. }
            | Self::Closed { .. }
            | Self::NoLiveSession { .. }
            | Self::TakeNeedsPerson
            | Self::ConflictChanged { .. }
            | Self::EventLimit { .. } => ErrorKind::Conflict,
            Self::RemoteHuman => ErrorKind::PermissionDenied,
            Self::InvalidPriority { .. }
            | Self::InvalidReason
            | Self::TargetRequired
            | Self::WhichAssignment { .. } => ErrorKind::InvalidArgument,
            Self::Revision(error) => error.kind(),
            Self::Db(error) => error.kind(),
            Self::Read(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for AssignError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::NotFound { .. } => Some(Advice::run(
                "List the open assignments.",
                ["reef", "conflict", "assignments"],
            )),
            Self::QueueEmpty => Some(Advice::run(
                "Nothing waits for a resolver; list every open assignment.",
                ["reef", "conflict", "assignments"],
            )),
            Self::WhichAssignment { .. } => Some(Advice::run(
                "Name the assignment; these are the ones your workspace owns.",
                ["reef", "conflict", "assignments", "--mine"],
            )),
            Self::Held { assignment, .. } => Some(Advice::run(
                "Another session or a person holds it. A person can reassign it to the queue or to your workspace.",
                ["reef", "conflict", "assign", assignment.as_str(), "--queue"],
            )),
            Self::Unowned { assignment } => Some(Advice::run(
                "A person decides first: queue it, assign it, or take it over.",
                ["reef", "conflict", "assign", assignment.as_str(), "--queue"],
            )),
            Self::Closed { .. } => Some(Advice::hint(
                "The change no longer carries the conflict; nothing to resolve.",
            )),
            Self::NoLiveSession { .. } => Some(Advice::hint(
                "Name an active workspace: claims are held by a workspace's session. `reef conflict claim --next` creates one.",
            )),
            Self::TakeNeedsPerson => Some(Advice::run(
                "Claim it from the queue instead.",
                ["reef", "conflict", "claim", "--next"],
            )),
            Self::RemoteHuman => Some(Advice::hint(
                "Take it over on the daemon's machine; through the remote listener, claim it for a workspace.",
            )),
            Self::ConflictChanged { .. } => Some(Advice::hint(
                "Read the conflict again (`reef conflict show`), then retry with its id.",
            )),
            Self::EventLimit { assignment } => Some(Advice::run(
                "A person takes it over instead.",
                ["reef", "conflict", "take", assignment.as_str()],
            )),
            _ => None,
        }
    }
}

impl AssignError {
    /// Its reason and metadata, as `crate::error` reports them.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            Self::NotFound { assignment } => (
                reason::ASSIGNMENT_NOT_FOUND,
                vec![("assignment", assignment.clone())],
            ),
            Self::QueueEmpty => (reason::QUEUE_EMPTY, Vec::new()),
            Self::WhichAssignment { owned } => (
                reason::ASSIGNMENT_REQUIRED,
                vec![("owned", owned.join(","))],
            ),
            Self::Held {
                state,
                owner_workspace,
                owner_agent,
                ..
            } => (
                reason::ASSIGNMENT_HELD,
                vec![
                    ("state", state.clone()),
                    ("owner_workspace", owner_workspace.clone()),
                    ("owner_agent", owner_agent.clone()),
                ],
            ),
            Self::Unowned { assignment } => (
                reason::ASSIGNMENT_UNOWNED,
                vec![("assignment", assignment.clone())],
            ),
            Self::Closed { assignment } => (
                reason::ASSIGNMENT_CLOSED,
                vec![("assignment", assignment.clone())],
            ),
            Self::NoLiveSession { workspace } => (
                reason::NO_LIVE_SESSION,
                vec![("workspace_id", workspace.clone())],
            ),
            Self::TakeNeedsPerson => (reason::TAKE_NEEDS_PERSON, Vec::new()),
            Self::RemoteHuman => (reason::REMOTE_HUMAN_REQUIRED, Vec::new()),
            Self::ConflictChanged { conflict_id } => (
                reason::CONFLICT_CHANGED,
                vec![("conflict_id", conflict_id.clone())],
            ),
            Self::EventLimit { .. } => (
                reason::ASSIGNMENT_EVENT_LIMIT,
                vec![("max", MAX_EVENTS.to_string())],
            ),
            Self::InvalidPriority { .. } => (
                reason::INVALID_PRIORITY,
                vec![
                    ("min", MIN_PRIORITY.to_string()),
                    ("max", MAX_PRIORITY.to_string()),
                ],
            ),
            Self::InvalidReason => (reason::INVALID_REASON, Vec::new()),
            Self::TargetRequired => (reason::ASSIGNMENT_TARGET_REQUIRED, Vec::new()),
            _ => return None,
        })
    }
}

/// What one repository's last look saw.
#[derive(Debug, Default)]
struct Looked {
    /// The head operation it read.
    op: Option<String>,
    /// It already warned that [`MAX_OPEN`] was reached.
    warned_full: bool,
}

/// The per-repository assignment locks, and the follower's wake-ups; one in
/// [`AppState`].
#[derive(Debug)]
pub struct Assignments {
    repos: Mutex<HashMap<RepoId, Arc<tokio::sync::Mutex<Looked>>>>,
    grace: Mutex<Duration>,
    deadline: Mutex<Option<Timestamp>>,
    wake: tokio::sync::Notify,
}

impl Default for Assignments {
    fn default() -> Assignments {
        Assignments {
            repos: Mutex::new(HashMap::new()),
            grace: Mutex::new(OWNER_GRACE),
            deadline: Mutex::new(None),
            wake: tokio::sync::Notify::new(),
        }
    }
}

impl Assignments {
    fn lock(&self, repo: RepoId) -> Arc<tokio::sync::Mutex<Looked>> {
        let mut repos = self.repos.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(repos.entry(repo).or_default())
    }

    /// How long an ended owner keeps its assignment.
    pub fn grace(&self) -> Duration {
        *self.grace.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Sets the grace (tests, which never sleep through five minutes).
    #[doc(hidden)]
    pub fn set_grace(&self, grace: Duration) {
        *self.grace.lock().unwrap_or_else(PoisonError::into_inner) = grace;
        self.wake.notify_one();
    }

    /// The earliest grace deadline the last looks saw.
    fn deadline(&self) -> Option<Timestamp> {
        *self.deadline.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn note_deadline(&self, at: Option<Timestamp>) {
        let mut deadline = self.deadline.lock().unwrap_or_else(PoisonError::into_inner);
        *deadline = match (*deadline, at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
    }

    fn clear_deadline(&self) {
        *self.deadline.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    /// Wakes the follower.
    pub fn wake(&self) {
        self.wake.notify_one();
    }
}

/// `reason` is one a caller may give.
pub fn valid_reason(reason: &str) -> bool {
    crate::lifecycle::valid_reason(reason)
}

fn grace_millis(grace: Duration) -> i64 {
    i64::try_from(grace.as_millis()).unwrap_or(i64::MAX)
}

fn ago(now: Timestamp, span: Duration) -> Timestamp {
    Timestamp(now.0.saturating_sub(grace_millis(span)))
}

/// An owner from provenance session `row`.
fn owner_of(state: &AppState, row: &ProvenanceSessionRow) -> OwnerColumns {
    let workspace_owner = row
        .workspace_id
        .parse::<WorkspaceId>()
        .ok()
        .and_then(|id| state.db.workspace(id).ok())
        .map(|record| record.owner);
    OwnerColumns {
        session_id: Some(row.session_id.clone()),
        workspace_id: Some(row.workspace_id.clone()),
        workspace_owner,
        identity_id: row.identity_id.clone(),
        actor: Some(row.actor.clone()),
        agent: row.agent.clone(),
        client_session: row
            .declared
            .client_session
            .clone()
            .filter(|session| !session.is_empty()),
        ended_at: row.ended_at,
    }
}

/// The default owner of `change` (decision 2): the newest live session
/// whose records of it verify, else the newest such session (ended), else
/// the live session of the one active workspace editing it. Blocking.
pub fn default_owner(
    state: &AppState,
    repo: RepoId,
    change: &str,
) -> Result<Option<OwnerColumns>, DbError> {
    let db = &state.db;
    let filter = ProvenanceFilter {
        change_ids: vec![change.to_owned()],
        ..ProvenanceFilter::default()
    };
    let rows = db.list_provenance(repo, &filter, None, RECORDS_READ)?;
    let records = crate::provenance::read::read_records(db, rows, false)?;
    let mut order: Vec<String> = Vec::new();
    for record in records {
        if record.signature() != reef_api::v1::SignatureCheck::Valid
            || record.session_id.is_empty()
            || order.contains(&record.session_id)
        {
            continue;
        }
        order.push(record.session_id);
    }
    let mut sessions = Vec::with_capacity(order.len());
    for id in &order {
        if let Some(row) = db.provenance_session(id)? {
            if row.ended_at.is_none() {
                return Ok(Some(owner_of(state, &row)));
            }
            sessions.push(row);
        }
    }
    if let Some(newest) = sessions.first() {
        return Ok(Some(owner_of(state, newest)));
    }
    let editing: Vec<WorkspaceRecord> = db
        .workspaces(Some(repo), &[WorkspaceState::Active])?
        .into_iter()
        .filter(|record| record.change_id == change)
        .collect();
    if let [record] = editing.as_slice()
        && let Some(live) = db.live_provenance_session(&record.id.as_hex())?
    {
        return Ok(Some(owner_of(state, &live)));
    }
    Ok(None)
}

/// A live session of `repo` that declared `owner`'s client session, with
/// its identity, other than `owner`'s own session: one to follow or return
/// to. Blocking.
fn follower_of(
    state: &AppState,
    repo: RepoId,
    owner: &OwnerColumns,
) -> Result<Option<OwnerColumns>, DbError> {
    let Some(client) = owner.client_session.as_deref().filter(|c| !c.is_empty()) else {
        return Ok(None);
    };
    let found = state
        .db
        .live_sessions_by_client(repo, client)?
        .into_iter()
        .find(|row| {
            row.identity_id == owner.identity_id
                && Some(&row.session_id) != owner.session_id.as_ref()
        });
    Ok(found.map(|row| owner_of(state, &row)))
}

/// The event of `before` becoming `after`, for `reason`.
fn event_of(
    before: Option<&AssignmentRow>,
    after: &AssignmentRow,
    reason: Reason,
    note: Option<&str>,
    caller: &Caller,
    at: Timestamp,
) -> AssignmentEventRow {
    AssignmentEventRow {
        repo_id: after.repo_id,
        id: crate::identity::new_id("cae_"),
        seq: 0,
        assignment_id: after.id.clone(),
        change_id: after.change_id.clone(),
        conflict_id: after.conflict_id.clone(),
        from_state: before.map(|row| row.state.clone()),
        to_state: after.state.clone(),
        reason: reason.as_str().to_owned(),
        owner: after.owner.clone(),
        from_workspace_id: before
            .and_then(|row| row.owner.workspace_id.clone())
            .filter(|workspace| after.owner.workspace_id.as_ref() != Some(workspace)),
        priority: after.priority,
        note: note.filter(|note| !note.is_empty()).map(str::to_owned),
        by_actor: caller.actor.clone(),
        by_agent: caller.agent.clone(),
        by_identity_id: caller.identity_id.clone(),
        by_session_id: caller.session_id.clone(),
        by_remote_token: caller.remote.as_ref().map(|(id, _)| id.clone()),
        at,
    }
}

/// `row` moved to `state` for `reason` with `owner`, at `at`: one more
/// event, queued now when it enters the queue.
fn moved(
    row: &AssignmentRow,
    state: State,
    reason: Reason,
    owner: OwnerColumns,
    at: Timestamp,
) -> AssignmentRow {
    let mut next = row.clone();
    if state == State::Queued && row.state != State::Queued.as_str() {
        next.queued_at = Some(at);
    }
    state.as_str().clone_into(&mut next.state);
    reason.as_str().clone_into(&mut next.reason);
    next.owner = owner;
    next.events = row.events.saturating_add(1);
    next.updated_at = at;
    next
}

/// One change of a row, with its event.
type Change = (AssignmentRow, AssignmentEventRow);

/// What a look read from jj.
struct Scan {
    found: Vec<Conflicted>,
    more: bool,
    gone: HashMap<ChangeIdHex, Option<Visible>>,
}

/// Brings `repo`'s assignments up to date with its head, under its
/// assignment lock; announces what changed.
pub async fn look(state: &Arc<AppState>, repo: RepoId) -> Result<(), AssignError> {
    let lock = state.assignments.lock(repo);
    let mut looked = lock.lock().await;
    look_locked(state, repo, &mut looked).await
}

/// [`look`] at the head the repository thread has loaded, without loading
/// another process's operations (which would merge concurrent operation
/// heads that nothing asked to merge): the follower's look. Outside writes
/// are looked at once the daemon next loads them, as any read does.
pub async fn look_loaded(state: &Arc<AppState>, repo: RepoId) -> Result<(), AssignError> {
    let lock = state.assignments.lock(repo);
    let mut looked = lock.lock().await;
    look_at(state, repo, &mut looked, false).await
}

async fn look_locked(
    state: &Arc<AppState>,
    repo: RepoId,
    looked: &mut Looked,
) -> Result<(), AssignError> {
    look_at(state, repo, looked, true).await
}

/// A look at the head, reloaded first when `reload` (as reads do).
async fn look_at(
    state: &Arc<AppState>,
    repo: RepoId,
    looked: &mut Looked,
    reload: bool,
) -> Result<(), AssignError> {
    let handle = state
        .manager
        .handle(repo)
        .map_err(|error| AssignError::Read(Box::new(error)))?;
    let head = if reload {
        state.reads.current(&handle).await
    } else {
        state.reads.loaded(&handle).await
    }
    .map_err(|error| AssignError::Read(Box::new(error)))?;
    let op = oplog::op_id_of(&head).to_string();
    let now = Timestamp::now();
    if looked.op.as_deref() != Some(op.as_str()) {
        let open = state.db.open_assignments(repo)?;
        let record = state.db.repository(repo)?;
        let user_email = state.settings.user_email().to_owned();
        let open_changes: Vec<ChangeIdHex> = open
            .iter()
            .filter(|row| row.kind == KIND_TREE)
            .filter_map(|row| row.change_id.parse().ok())
            .collect();
        let (stop, _guard) = state.reads.stop();
        let scan = state
            .reads
            .run(&handle, move || async move {
                let scope = RevsetScope {
                    workspace: None,
                    user_email: &user_email,
                    root: &record.root,
                };
                let parsed = revset::parse(&head, CONFLICTS, &scope)?;
                let (found, more) =
                    jj_assignment::conflicted_changes(&head, &parsed, SCAN_CAP, &|| {
                        stop.requested()
                    })
                    .await?;
                let seen: HashSet<&ChangeIdHex> = found.iter().map(|c| &c.change).collect();
                let mut gone = HashMap::new();
                for change in open_changes {
                    if !seen.contains(&change) {
                        let visible = jj_assignment::visible_commit(&head, &change).await?;
                        gone.insert(change, visible);
                    }
                }
                Ok::<_, AssignError>(Scan { found, more, gone })
            })
            .await
            .map_err(|error| AssignError::Read(Box::new(error)))??;
        let warned = looked.warned_full;
        let (changes, warned) = blocking(state, move |state| {
            let mut looked = Looked {
                op: None,
                warned_full: warned,
            };
            let changes = reconcile(state, repo, &open, &scan, now, &mut looked)?;
            Ok((changes, looked.warned_full))
        })
        .await?;
        looked.warned_full = warned;
        let announced = record_and_announce(state, repo, changes, None, None).await?;
        // The follower's own operations need no second reading of jj.
        looked.op = Some(announced.after(&op).unwrap_or(op));
    }
    let changes = blocking(state, move |state| liveness(state, repo, now)).await?;
    let announced = record_and_announce(state, repo, changes, None, None).await?;
    if let Some(head) = looked.op.clone()
        && let Some(after) = announced.after(&head)
    {
        looked.op = Some(after);
    }
    Ok(())
}

/// Runs `work` on the blocking pool: the follower's database and
/// provenance reads, which verify signatures.
async fn blocking<T: Send + 'static>(
    state: &Arc<AppState>,
    work: impl FnOnce(&AppState) -> Result<T, AssignError> + Send + 'static,
) -> Result<T, AssignError> {
    let state = Arc::clone(state);
    tokio::task::spawn_blocking(move || work(&state))
        .await
        .map_err(|error| AssignError::Read(Box::new(error)))?
}

/// The rows a look's jj reading opens, changes and closes.
fn reconcile(
    state: &AppState,
    repo: RepoId,
    open: &[AssignmentRow],
    scan: &Scan,
    now: Timestamp,
    looked: &mut Looked,
) -> Result<Vec<Change>, AssignError> {
    let db = &state.db;
    let daemon = Caller::daemon();
    let mut by_change: HashMap<&str, &AssignmentRow> = open
        .iter()
        .filter(|row| row.kind == KIND_TREE)
        .map(|row| (row.change_id.as_str(), row))
        .collect();
    let mut open_count = open.len();
    let mut changes = Vec::new();
    for found in &scan.found {
        let change = found.change.to_string();
        let commit = found.commit.to_string();
        match by_change.remove(change.as_str()) {
            Some(row) if found.introduced == 0 => {
                let mut next = moved(
                    row,
                    State::Closed,
                    Reason::Inherited,
                    row.owner.clone(),
                    now,
                );
                next.closed_at = Some(now);
                next.closed_commit_id = Some(commit);
                next.commit_id = found.commit.to_string();
                changes.push((
                    next.clone(),
                    event_of(Some(row), &next, Reason::Inherited, None, &daemon, now),
                ));
            }
            Some(row) if row.conflict_id != found.conflict_id => {
                let mut next = row.clone();
                next.commit_id = commit;
                next.conflict_id.clone_from(&found.conflict_id);
                next.introduced = found.introduced;
                next.files_total = found.files_total;
                next.conflict_changed_at = now;
                // At most one `changed` event a minute per assignment
                // (S-153): a resolver snapshotting through a wide conflict
                // changes its id at every snapshot; the rest are quiet.
                if now.0.saturating_sub(row.updated_at.0) < grace_millis(CHANGED_EVERY) {
                    db.save_assignment(&next)?;
                } else {
                    next.events = row.events.saturating_add(1);
                    next.updated_at = now;
                    changes.push((
                        next.clone(),
                        event_of(Some(row), &next, Reason::Changed, None, &daemon, now),
                    ));
                }
            }
            Some(row) => {
                if row.commit_id != commit || row.introduced != found.introduced {
                    let mut next = row.clone();
                    next.commit_id = commit;
                    next.introduced = found.introduced;
                    next.files_total = found.files_total;
                    db.save_assignment(&next)?;
                }
            }
            None if found.introduced == 0 => {}
            None => {
                if open_count >= MAX_OPEN {
                    if !looked.warned_full {
                        tracing::warn!(%repo, max = MAX_OPEN, "too many open conflict assignments; leaving the rest unassigned");
                        looked.warned_full = true;
                    }
                    continue;
                }
                open_count += 1;
                changes.push(open_one(state, repo, found, now)?);
            }
        }
    }
    if open_count < MAX_OPEN {
        looked.warned_full = false;
    }
    changes.extend(closes(state, repo, by_change.into_values(), scan, now)?);
    Ok(changes)
}

/// The rows of `rows` whose change the scan did not find conflicted, closed
/// with their reasons.
fn closes<'a>(
    state: &AppState,
    repo: RepoId,
    rows: impl Iterator<Item = &'a AssignmentRow>,
    scan: &Scan,
    now: Timestamp,
) -> Result<Vec<Change>, AssignError> {
    let daemon = Caller::daemon();
    let mut changes = Vec::new();
    for row in rows {
        let Ok(change) = row.change_id.parse::<ChangeIdHex>() else {
            continue;
        };
        let (reason, closed_commit) = match scan.gone.get(&change) {
            Some(None) => (Reason::Abandoned, None),
            Some(Some(visible)) if !visible.conflicted => {
                (Reason::Resolved, Some(visible.commit.to_string()))
            }
            // Conflicted but not in `mutable() & conflicts()`: immutable,
            // unless the scan stopped short of it.
            Some(Some(visible)) if !scan.more => {
                (Reason::Immutable, Some(visible.commit.to_string()))
            }
            _ => continue,
        };
        let resolved_session = match (&closed_commit, reason) {
            (Some(commit), Reason::Resolved) => resolver_of(state, repo, commit, row.opened_at)?,
            _ => None,
        };
        let mut next = moved(row, State::Closed, reason, row.owner.clone(), now);
        next.closed_at = Some(now);
        next.closed_commit_id = closed_commit;
        next.resolved_session_id = resolved_session;
        changes.push((
            next.clone(),
            event_of(Some(row), &next, reason, None, &daemon, now),
        ));
    }
    Ok(changes)
}

/// The session whose provenance record names `commit`, when that record
/// verifies and was made after the assignment opened (`opened_at`): an undo
/// that brings back a commit from before the conflict names no resolver.
/// Blocking.
fn resolver_of(
    state: &AppState,
    repo: RepoId,
    commit: &str,
    opened_at: Timestamp,
) -> Result<Option<String>, DbError> {
    let Some(stored) = state.db.provenance_by_commit(repo, commit)? else {
        return Ok(None);
    };
    if stored.row.recorded_at < opened_at {
        return Ok(None);
    }
    let session = stored.row.session_id.clone();
    let verified = crate::provenance::read::read_records(&state.db, vec![stored], false)?
        .into_iter()
        .any(|record| record.signature() == reef_api::v1::SignatureCheck::Valid);
    Ok(verified.then_some(session))
}

/// A new (or reopened) assignment for `found`, with its default owner.
fn open_one(
    state: &AppState,
    repo: RepoId,
    found: &Conflicted,
    now: Timestamp,
) -> Result<Change, AssignError> {
    let change = found.change.to_string();
    let owner = default_owner(state, repo, &change)?;
    let (next_state, reason) = match &owner {
        Some(_) => (State::Assigned, Reason::Default),
        None => (State::Unowned, Reason::NoOwner),
    };
    let owner = owner.unwrap_or_default();
    let daemon = Caller::daemon();
    let reopen = state.db.reopenable_assignment(
        repo,
        &change,
        KIND_TREE,
        &found.conflict_id,
        ago(now, REOPEN_WINDOW),
    )?;
    if let Some(closed) = reopen {
        let mut next = moved(&closed, next_state, reason, owner, now);
        next.commit_id = found.commit.to_string();
        next.introduced = found.introduced;
        next.files_total = found.files_total;
        next.closed_at = None;
        next.closed_commit_id = None;
        next.resolved_session_id = None;
        let event = event_of(Some(&closed), &next, Reason::Reopened, None, &daemon, now);
        return Ok((next, event));
    }
    let row = AssignmentRow {
        repo_id: repo,
        id: crate::identity::new_id("asg_"),
        change_id: change,
        kind: KIND_TREE.to_owned(),
        commit_id: found.commit.to_string(),
        conflict_id: found.conflict_id.clone(),
        first_conflict_id: found.conflict_id.clone(),
        introduced: found.introduced,
        files_total: found.files_total,
        state: next_state.as_str().to_owned(),
        reason: reason.as_str().to_owned(),
        owner,
        priority: 0,
        queued_at: None,
        claims: 0,
        events: 1,
        opened_at: now,
        updated_at: now,
        conflict_changed_at: now,
        closed_at: None,
        closed_commit_id: None,
        resolved_session_id: None,
    };
    let event = event_of(None, &row, reason, None, &daemon, now);
    Ok((row, event))
}

/// The rows the owners' liveness moves: ended owners followed, or queued
/// after their grace (or unowned, when Reef never recorded the change), and
/// queued ones returned. An unowned row stays so until a caller moves it.
/// Notes the earliest grace deadline left. Blocking.
fn liveness(state: &AppState, repo: RepoId, now: Timestamp) -> Result<Vec<Change>, AssignError> {
    let db = &state.db;
    let daemon = Caller::daemon();
    let grace = grace_millis(state.assignments.grace());
    let open = db.open_assignments(repo)?;
    let workspaces: Vec<&str> = open
        .iter()
        .filter(|row| row.state == State::Assigned.as_str())
        .filter_map(|row| row.owner.workspace_id.as_deref())
        .collect();
    let live = db.live_provenance_sessions(&workspaces)?;
    let mut changes = Vec::new();
    let mut deadline: Option<Timestamp> = None;
    for row in &open {
        match State::parse(&row.state) {
            Some(State::Assigned) => {
                let Some(workspace) = row.owner.workspace_id.as_deref() else {
                    continue;
                };
                if let Some((session, _)) = live.get(workspace) {
                    if row.owner.session_id.as_ref() != Some(&session.session_id)
                        || row.owner.ended_at.is_some()
                    {
                        // Restarted after a revocation: the same owner.
                        let mut next = row.clone();
                        next.owner = owner_of(state, session);
                        db.save_assignment(&next)?;
                    }
                    continue;
                }
                let ended = match row.owner.ended_at {
                    Some(at) => at,
                    None => row
                        .owner
                        .session_id
                        .as_deref()
                        .map(|id| db.provenance_session(id))
                        .transpose()?
                        .flatten()
                        .and_then(|session| session.ended_at)
                        .unwrap_or(now),
                };
                let mut ended_owner = row.owner.clone();
                ended_owner.ended_at = Some(ended);
                if let Some(follower) = follower_of(state, repo, &ended_owner)? {
                    let next = moved(row, State::Assigned, Reason::Followed, follower, now);
                    changes.push((
                        next.clone(),
                        event_of(Some(row), &next, Reason::Followed, None, &daemon, now),
                    ));
                } else if now.0 >= ended.0.saturating_add(grace) {
                    // An owner that only edited a change Reef never recorded
                    // (decision 2's third case) hands it back to nobody, not
                    // to the queue: it may be a person's (S-155).
                    let (to, why) = if db.provenance_count_by_change(repo, &row.change_id)? == 0 {
                        (State::Unowned, Reason::NoOwner)
                    } else {
                        (State::Queued, Reason::OwnerEnded)
                    };
                    let next = moved(row, to, why, ended_owner, now);
                    changes.push((
                        next.clone(),
                        event_of(Some(row), &next, why, None, &daemon, now),
                    ));
                } else {
                    let due = Timestamp(ended.0.saturating_add(grace));
                    deadline = Some(deadline.map_or(due, |d| d.min(due)));
                    if row.owner.ended_at.is_none() {
                        let mut next = row.clone();
                        next.owner = ended_owner;
                        db.save_assignment(&next)?;
                    }
                }
            }
            Some(State::Queued) if row.reason == Reason::OwnerEnded.as_str() => {
                if let Some(owner) = follower_of(state, repo, &row.owner)? {
                    let next = moved(row, State::Assigned, Reason::Returned, owner, now);
                    changes.push((
                        next.clone(),
                        event_of(Some(row), &next, Reason::Returned, None, &daemon, now),
                    ));
                }
            }
            _ => {}
        }
    }
    state.assignments.note_deadline(deadline);
    Ok(changes)
}

/// Records `changes` and announces their events in operations of their own
/// (for `workspace` and `agent`, when a caller made them); the last
/// operation's id. A failure to announce is logged: the rows are the
/// truth.
async fn record_and_announce(
    state: &AppState,
    repo: RepoId,
    changes: Vec<Change>,
    workspace: Option<&str>,
    agent: Option<&str>,
) -> Result<Announced, AssignError> {
    if changes.is_empty() {
        return Ok(Announced::default());
    }
    let events = state.db.record_assignments(repo, changes)?;
    Ok(announce(state, repo, &events, workspace, agent).await)
}

/// The operations [`announce`] wrote.
#[derive(Debug, Default)]
struct Announced {
    /// The head before the first one.
    from: Option<String>,
    /// The last one.
    last: Option<OperationIdHex>,
    /// Each was written on the one before, with nothing between.
    contiguous: bool,
}

impl Announced {
    /// The head after these operations, when they were written right on
    /// `head` with nothing between: a look need not read jj again for them.
    fn after(&self, head: &str) -> Option<String> {
        (self.contiguous && self.from.as_deref() == Some(head))
            .then(|| self.last.as_ref().map(ToString::to_string))
            .flatten()
    }
}

/// The notice an operation carries for `event`.
fn notice(event: &AssignmentEventRow) -> AssignmentNotice {
    AssignmentNotice {
        id: event.id.clone(),
        seq: u64::try_from(event.seq).unwrap_or(0),
        assignment: event.assignment_id.clone(),
        change: event.change_id.clone(),
        conflict: event.conflict_id.clone(),
        from: event.from_state.clone().unwrap_or_default(),
        to: event.to_state.clone(),
        reason: event.reason.clone(),
        workspace: event.owner.workspace_id.clone().unwrap_or_default(),
        agent: event.owner.agent.clone().unwrap_or_default(),
        from_workspace: event.from_workspace_id.clone().unwrap_or_default(),
        priority: event.priority,
    }
}

async fn announce(
    state: &AppState,
    repo: RepoId,
    events: &[AssignmentEventRow],
    workspace: Option<&str>,
    agent: Option<&str>,
) -> Announced {
    let handle = match state.manager.handle(repo) {
        Ok(handle) => handle,
        Err(error) => {
            tracing::warn!(%repo, error = %ErrorChain(&error), "cannot announce conflict assignment events");
            return Announced::default();
        }
    };
    let mut announced = Announced {
        contiguous: true,
        ..Announced::default()
    };
    for chunk in events.chunks(jj_assignment::MAX_NOTICES) {
        let description = match chunk {
            [one] => format!(
                "reef: conflict assignment {} {}",
                one.to_state, one.change_id
            ),
            many => format!("reef: conflict assignments ({})", many.len()),
        };
        let mut meta = OpMeta::new(OpKind::ConflictAssignment, description);
        if let Some(workspace) = workspace {
            meta = meta.workspace(workspace);
        }
        if let Some(agent) = agent {
            meta = meta.agent(agent);
        }
        meta.conflict_events = chunk.iter().map(notice).collect();
        let written = handle
            .call_async(move |head| {
                async move {
                    head.refresh()
                        .await
                        .map_err(|error| AssignError::Read(Box::new(error)))?;
                    let before = oplog::op_id_of(&head.repo.readonly()).to_string();
                    let op = head
                        .repo
                        .record_assignment_events(&meta)
                        .await
                        .map_err(|error| AssignError::Read(Box::new(error)))?;
                    Ok::<_, AssignError>((before, op))
                }
                .boxed_local()
            })
            .await;
        match written {
            Ok(Ok((before, op))) => {
                match &announced.last {
                    None => announced.from = Some(before),
                    Some(last) if last.to_string() != before => announced.contiguous = false,
                    Some(_) => {}
                }
                announced.last = Some(op);
            }
            Ok(Err(error)) => {
                tracing::warn!(%repo, error = %ErrorChain(&error), "cannot announce conflict assignment events; the rows stand");
            }
            Err(error) => {
                tracing::warn!(%repo, error = %ErrorChain(&error), "cannot announce conflict assignment events; the rows stand");
            }
        }
    }
    announced
}

/// The row `named` names in `repo`, open or not.
fn find(state: &AppState, repo: RepoId, named: &Named) -> Result<AssignmentRow, AssignError> {
    let found = match named {
        Named::Id(id) => state.db.assignment(repo, id)?,
        Named::Change(change) => state.db.assignment_of_change(repo, change)?,
        Named::QueueHead => {
            return state.db.queue_head(repo)?.ok_or(AssignError::QueueEmpty);
        }
    };
    found.ok_or_else(|| AssignError::NotFound {
        assignment: match named {
            Named::Id(id) | Named::Change(id) => id.clone(),
            Named::QueueHead => String::new(),
        },
    })
}

/// Refuses a change to `row` when it is closed, its conflict is not
/// `expected`, or (when `capped`) it has [`MAX_EVENTS`] events. A person's
/// takeover is not capped, so an assignment at the cap can still be moved.
fn check_open(row: &AssignmentRow, expected: &str, capped: bool) -> Result<(), AssignError> {
    if row.state == State::Closed.as_str() {
        return Err(AssignError::Closed {
            assignment: row.id.clone(),
        });
    }
    if !expected.is_empty() && expected != row.conflict_id {
        return Err(AssignError::ConflictChanged {
            conflict_id: row.conflict_id.clone(),
        });
    }
    if capped && row.events >= MAX_EVENTS {
        return Err(AssignError::EventLimit {
            assignment: row.id.clone(),
        });
    }
    Ok(())
}

fn held(row: &AssignmentRow) -> AssignError {
    AssignError::Held {
        assignment: row.id.clone(),
        state: row.state.clone(),
        owner_workspace: row.owner.workspace_id.clone().unwrap_or_default(),
        owner_agent: row
            .owner
            .agent
            .clone()
            .or_else(|| row.owner.identity_id.clone())
            .unwrap_or_default(),
    }
}

/// The owner a claim or an assignment to `workspace` makes: its live
/// session.
fn live_owner(state: &AppState, workspace: &WorkspaceRecord) -> Result<OwnerColumns, AssignError> {
    let none = || AssignError::NoLiveSession {
        workspace: workspace.id.as_hex(),
    };
    if workspace.state != WorkspaceState::Active {
        return Err(none());
    }
    let session = state
        .db
        .live_provenance_session(&workspace.id.as_hex())?
        .ok_or_else(none)?;
    Ok(owner_of(state, &session))
}

/// R3.28's gate: whether `caller` may claim `row`. Allows every claim;
/// R3.28 refuses an agent's claim of an `ask` tier conflict, or one over
/// its attempt cap, here.
pub fn may_claim(_row: &AssignmentRow, _caller: &Caller) -> Result<(), AssignError> {
    Ok(())
}

/// What a write did.
#[derive(Debug)]
pub struct Outcome {
    /// The assignment afterwards.
    pub row: AssignmentRow,
    /// The operation that announced it; `None` when nothing changed.
    pub op_id: Option<OperationIdHex>,
}

/// Claims `named` for `workspace`'s live session (decision 6).
pub async fn claim(
    state: &Arc<AppState>,
    workspace: &WorkspaceRecord,
    named: &Named,
    expected: &str,
    caller: &Caller,
) -> Result<Outcome, AssignError> {
    let repo = workspace.repo_id;
    let lock = state.assignments.lock(repo);
    let mut looked = lock.lock().await;
    look_locked(state, repo, &mut looked).await?;
    let owner = live_owner(state, workspace)?;
    let row = find(state, repo, named)?;
    if row.state == State::Assigned.as_str() && row.owner.session_id == owner.session_id {
        return Ok(Outcome { row, op_id: None });
    }
    check_open(&row, expected, true)?;
    match State::parse(&row.state) {
        Some(State::Queued) => {}
        Some(State::Unowned) => {
            return Err(AssignError::Unowned {
                assignment: row.id.clone(),
            });
        }
        _ => return Err(held(&row)),
    }
    may_claim(&row, caller)?;
    let now = Timestamp::now();
    let mut next = moved(&row, State::Assigned, Reason::Claimed, owner, now);
    next.claims = row.claims.saturating_add(1);
    let event = event_of(Some(&row), &next, Reason::Claimed, None, caller, now);
    let op_id = record_and_announce(
        state,
        repo,
        vec![(next.clone(), event)],
        Some(&workspace.id.as_hex()),
        caller.marked_agent.as_deref(),
    )
    .await?
    .last;
    Ok(Outcome { row: next, op_id })
}

/// Releases `named` to the queue (decision 6): from the workspace whose
/// session owns it, or from the person who took it.
pub async fn release(
    state: &Arc<AppState>,
    repo: RepoId,
    workspace: Option<&WorkspaceRecord>,
    named: &Named,
    reason: &str,
    caller: &Caller,
) -> Result<Outcome, AssignError> {
    if !valid_reason(reason) {
        return Err(AssignError::InvalidReason);
    }
    let lock = state.assignments.lock(repo);
    let mut looked = lock.lock().await;
    look_locked(state, repo, &mut looked).await?;
    let row = find(state, repo, named)?;
    check_open(&row, "", true)?;
    let owns = match State::parse(&row.state) {
        Some(State::Assigned) => workspace.is_some_and(|workspace| {
            row.owner.workspace_id.as_deref() == Some(workspace.id.as_hex().as_str())
        }),
        Some(State::Taken) => {
            caller.remote.is_none()
                && row.owner.identity_id.is_some()
                && row.owner.identity_id == caller.person_identity_id
        }
        _ => false,
    };
    if !owns {
        return Err(held(&row));
    }
    let now = Timestamp::now();
    let next = moved(
        &row,
        State::Queued,
        Reason::Released,
        row.owner.clone(),
        now,
    );
    let event = event_of(
        Some(&row),
        &next,
        Reason::Released,
        Some(reason),
        caller,
        now,
    );
    let op_id = record_and_announce(
        state,
        repo,
        vec![(next.clone(), event)],
        workspace.map(|w| w.id.as_hex()).as_deref(),
        caller.marked_agent.as_deref(),
    )
    .await?
    .last;
    Ok(Outcome { row: next, op_id })
}

/// Where [`assign`] puts an assignment.
#[derive(Debug)]
pub enum Target {
    /// To this workspace's live session.
    Workspace(Box<WorkspaceRecord>),
    /// Into the queue.
    Queue,
    /// A person takes it over.
    Take,
    /// Only the priority changes.
    Priority,
}

/// What [`assign`] is asked.
#[derive(Debug)]
pub struct AssignRequest<'a> {
    /// The assignment.
    pub named: &'a Named,
    /// The conflict id the caller read; empty for no check.
    pub expected: &'a str,
    /// Where it goes.
    pub target: &'a Target,
    /// The new priority, if any.
    pub priority: Option<i32>,
    /// Why.
    pub reason: &'a str,
}

/// Assigns an assignment (decision 7).
pub async fn assign(
    state: &Arc<AppState>,
    repo: RepoId,
    request: AssignRequest<'_>,
    caller: &Caller,
) -> Result<Outcome, AssignError> {
    let AssignRequest {
        named,
        expected,
        target,
        priority,
        reason,
    } = request;
    if !valid_reason(reason) {
        return Err(AssignError::InvalidReason);
    }
    if let Some(priority) = priority
        && !(MIN_PRIORITY..=MAX_PRIORITY).contains(&priority)
    {
        return Err(AssignError::InvalidPriority { priority });
    }
    if matches!(target, Target::Priority) && priority.is_none() {
        return Err(AssignError::TargetRequired);
    }
    if matches!(target, Target::Take) {
        if caller.remote.is_some() {
            return Err(AssignError::RemoteHuman);
        }
        if caller.actor != "human" {
            return Err(AssignError::TakeNeedsPerson);
        }
    }
    let lock = state.assignments.lock(repo);
    let mut looked = lock.lock().await;
    look_locked(state, repo, &mut looked).await?;
    let row = find(state, repo, named)?;
    check_open(&row, expected, !matches!(target, Target::Take))?;
    // An agent that assigns a conflict to a workspace is held to a claim's
    // rules (S-152): queued ones only, through R3.28's gate.
    if matches!(target, Target::Workspace(_)) && caller.actor != "human" {
        match State::parse(&row.state) {
            Some(State::Queued) => {}
            Some(State::Unowned) => {
                return Err(AssignError::Unowned {
                    assignment: row.id.clone(),
                });
            }
            _ => return Err(held(&row)),
        }
        may_claim(&row, caller)?;
    }
    let now = Timestamp::now();
    let (state_to, why, owner) = match target {
        Target::Workspace(workspace) => (
            State::Assigned,
            Reason::Assigned,
            live_owner(state, workspace)?,
        ),
        Target::Queue => (State::Queued, Reason::Assigned, row.owner.clone()),
        Target::Take => (
            State::Taken,
            Reason::Taken,
            OwnerColumns {
                identity_id: caller.person_identity_id.clone(),
                actor: Some("human".to_owned()),
                agent: caller.agent.clone(),
                ..OwnerColumns::default()
            },
        ),
        Target::Priority => (
            State::parse(&row.state).unwrap_or(State::Unowned),
            Reason::Assigned,
            row.owner.clone(),
        ),
    };
    let mut next = moved(&row, state_to, why, owner, now);
    if matches!(target, Target::Priority) {
        // Only the priority: the state and its reason stay.
        next.reason.clone_from(&row.reason);
    }
    if let Some(priority) = priority {
        next.priority = priority;
    }
    let event = event_of(Some(&row), &next, why, Some(reason), caller, now);
    let op_id = record_and_announce(
        state,
        repo,
        vec![(next.clone(), event)],
        caller.workspace.as_deref(),
        caller.marked_agent.as_deref(),
    )
    .await?
    .last;
    Ok(Outcome { row: next, op_id })
}

/// GC's tenth step: deletes `repo`'s assignments closed more than
/// [`RETENTION`] ago and before `keep_newer`, with their events; how many.
pub fn sweep(db: &crate::db::Db, repo: RepoId, keep_newer: Timestamp) -> Result<u64, DbError> {
    let before = ago(Timestamp::now(), RETENTION).min(keep_newer);
    db.delete_closed_assignments(repo, before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for state in [
            State::Assigned,
            State::Queued,
            State::Taken,
            State::Unowned,
            State::Closed,
        ] {
            assert_eq!(State::parse(state.as_str()), Some(state));
        }
        for reason in Reason::ALL {
            assert_eq!(Reason::parse(reason.as_str()), Some(reason));
        }
        assert_eq!(State::parse("nope"), None);
    }

    #[test]
    fn checks_refuse_closed_changed_and_full() {
        let row = AssignmentRow {
            repo_id: "a3dda866c7e8f343".parse().unwrap(),
            id: "asg_1".into(),
            change_id: "aa".into(),
            kind: KIND_TREE.into(),
            commit_id: "c".into(),
            conflict_id: "cfl_1".into(),
            first_conflict_id: "cfl_1".into(),
            introduced: 1,
            files_total: 1,
            state: "queued".into(),
            reason: "owner_ended".into(),
            owner: OwnerColumns::default(),
            priority: 0,
            queued_at: Some(Timestamp(1)),
            claims: 0,
            events: 1,
            opened_at: Timestamp(1),
            updated_at: Timestamp(1),
            conflict_changed_at: Timestamp(1),
            closed_at: None,
            closed_commit_id: None,
            resolved_session_id: None,
        };
        assert!(check_open(&row, "", true).is_ok());
        assert!(check_open(&row, "cfl_1", true).is_ok());
        assert!(matches!(
            check_open(&row, "cfl_2", true),
            Err(AssignError::ConflictChanged { .. })
        ));
        let full = AssignmentRow {
            events: MAX_EVENTS,
            ..row.clone()
        };
        assert!(matches!(
            check_open(&full, "", true),
            Err(AssignError::EventLimit { .. })
        ));
        // A person's takeover is not capped.
        assert!(check_open(&full, "", false).is_ok());
        let closed = AssignmentRow {
            state: "closed".into(),
            ..row
        };
        assert!(matches!(
            check_open(&closed, "", true),
            Err(AssignError::Closed { .. })
        ));
    }
}
