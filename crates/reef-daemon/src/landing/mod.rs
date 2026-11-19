//! The landing service, local (R3.13, ADR 0064; `docs/landing.md`).
//!
//! A client asks to land a proposed change ([`land_change`]). The gate
//! judges the commit the proposal recorded with the policy at the head of
//! its destination bookmark, refusing anything but `ALLOW`: a refusal is a
//! landing with outcome `blocked`, and the change stays proposed. A change
//! the gate passes is queued (R3.09's enqueue transition, cause
//! `landing`), and the repository's worker (`worker`) lands queued
//! changes one at a time: under the repository's create lock shared, it
//! previews and judges again at the destination's head, then, holding the
//! lifecycle lock, lands through [`crate::workspace::Manager::rewrite_held`]
//! in one operation that rebases the change when the destination moved,
//! gives workspaces on it a new working-copy commit, moves the bookmark and
//! records the `land` transition; or it returns the change to proposed (the
//! `dequeue` transition) with a reason. A landed change's satisfied rules'
//! evidence is pinned, and the policy bytes kept.
//!
//! `land_batch` takes a list so that stacks (R3.10) and batches (R3.14)
//! widen it; this ticket lands one change per landing.

pub(crate) mod automatic;
mod batch;
mod history;
pub(crate) mod instruct;
pub mod messages;
pub(crate) mod push;
mod worker;

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use reef_api::v1;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::land::{self, LandError, LandTarget, Preview};
use reef_lib::jj::lifecycle::{LifecycleState, TransitionCause};
use reef_lib::jj::log::{ChangeInfo, Describer};
use reef_lib::jj::oplog::{self, OpMeta, RemoteToken};
use reef_lib::jj::revset::{self, RevisionError, RevsetScope};
use reef_lib::jj::rewrite::{self, RewriteError};
use reef_lib::jj::{ChangeIdHex, CommitIdHex, OpKind, OperationIdHex, files};
use reef_policy::{Decision, Evaluation};

use crate::daemon::AppState;
use crate::db::{DbError, LandingChangeRow, LandingRow, StoredLanding, TransitionRow};
use crate::error::{Advice, Advise};
use crate::lifecycle::{Actor, LifecycleError};
use crate::model::{RepoId, Timestamp, WorkspaceRecord};
use crate::repo::RepoThreadError;
use crate::service::policy as policy_input;
use crate::workspace::ManagerError;

pub(crate) use automatic::start as start_automatic;
pub use automatic::{
    Automatic, MAX_OPEN_JOBS as AUTOMATIC_MAX_OPEN_JOBS, PER_PASS as AUTOMATIC_PER_PASS,
    PROPOSAL_PRIORITY, State as AutomaticState, Status as AutomaticStatus,
};
pub use batch::{AGENT as BATCH_AGENT, BATCH_PRIORITY, BATCH_QUEUE_TIMEOUT, MAX_TRIAL_ATTEMPTS};
pub use push::Pushes;
pub(crate) use worker::operation_end;
pub use worker::start;

/// The most changes a repository has queued at once.
pub const MAX_QUEUED: u64 = 1_000;

/// The longest a `LandChange` waits for its landing.
pub const MAX_WAIT: Duration = Duration::from_mins(10);

/// How many times a landing judges again because its destination moved
/// before it gives up (`busy`).
pub const MAX_ATTEMPTS: usize = 3;

/// The landing service's state; one in [`AppState`].
#[derive(Debug, Default)]
pub struct Landings {
    /// Repositories whose worker runs, each with whether it was woken while
    /// running (so it looks again before it stops).
    running: Mutex<HashMap<RepoId, bool>>,
    /// Woken whenever a landing finishes.
    finished: tokio::sync::Notify,
}

impl Landings {
    fn notify_finished(&self) {
        self.finished.notify_waiters();
    }
}

/// How a landing ended, or that it has not yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Refused at the gate.
    Blocked,
    /// Queued, not finished.
    Queued,
    /// Landed.
    Landed,
    /// Returned to proposed.
    Dequeued,
    /// A client moved the change while it was queued.
    Cancelled,
}

impl Outcome {
    /// The column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::Blocked => "blocked",
            Outcome::Queued => "queued",
            Outcome::Landed => "landed",
            Outcome::Dequeued => "dequeued",
            Outcome::Cancelled => "cancelled",
        }
    }

    /// The outcome a column's text names.
    pub fn parse(text: &str) -> Option<Outcome> {
        [
            Outcome::Blocked,
            Outcome::Queued,
            Outcome::Landed,
            Outcome::Dequeued,
            Outcome::Cancelled,
        ]
        .into_iter()
        .find(|outcome| outcome.as_str() == text)
    }
}

/// Why a landing was blocked or dequeued.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The decision was not `ALLOW`.
    Policy,
    /// The rebase conflicts, or the change does.
    Conflicted,
    /// A parent is not on the destination.
    UnlandedParents,
    /// The change moved off the queued commit.
    Amended,
    /// The destination is gone, conflicted or not local.
    Destination,
    /// The destination kept moving.
    Busy,
    /// An internal error.
    Failed,
    /// The change failed required checks on its batch's tested tree
    /// (R3.14).
    Checks,
    /// No verdict could be had on the batch's tested tree (R3.14).
    Unavailable,
    /// The change would land with nothing in it: the destination, or an
    /// earlier change of its batch, already has its edits (R3.16).
    Empty,
    /// The change edits a policy file above the bottom of its stack
    /// (R3.10): it lands on its own, after the changes below it.
    Alone,
    /// A change below it in its stack was declined or withdrawn (R3.11).
    Dependency,
}

impl Reason {
    /// The column's text.
    pub fn as_str(self) -> &'static str {
        match self {
            Reason::Policy => "policy",
            Reason::Conflicted => "conflicted",
            Reason::UnlandedParents => "unlanded_parents",
            Reason::Amended => "amended",
            Reason::Destination => "destination",
            Reason::Busy => "busy",
            Reason::Failed => "failed",
            Reason::Checks => "checks",
            Reason::Unavailable => "unavailable",
            Reason::Empty => "empty",
            Reason::Alone => "alone",
            Reason::Dependency => "dependency",
        }
    }

    /// The reason a column's text names.
    pub fn parse(text: &str) -> Option<Reason> {
        [
            Reason::Policy,
            Reason::Conflicted,
            Reason::UnlandedParents,
            Reason::Amended,
            Reason::Destination,
            Reason::Busy,
            Reason::Failed,
            Reason::Checks,
            Reason::Unavailable,
            Reason::Empty,
            Reason::Alone,
            Reason::Dependency,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == text)
    }
}

/// Why a landing could not be asked for or read.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LandingError {
    /// The change is not proposed or queued.
    #[error("change {change} is {state}; only a proposed change can land")]
    NotProposed {
        /// The change.
        change: String,
        /// Its state.
        state: LifecycleState,
        /// The head of the stack being landed, when the change is below it
        /// (R3.10): proposing the head proposes it.
        head: Option<String>,
    },
    /// A change below it in its stack was declined or withdrawn at its
    /// current commit (R3.11).
    #[error("change {change} below it was {state}; it cannot land on it")]
    DependencyLeftReview {
        /// The member that left review.
        change: String,
        /// `declined` or `withdrawn`.
        state: LifecycleState,
        /// The transition that took it out.
        transition: String,
        /// The `reef rebase` arguments that move the changes above it off
        /// it: the member above it, and where to.
        moves: (String, String),
    },
    /// The proposal's commit is not the change's visible one.
    #[error("change {change} moved from {commit}, the proposed commit, to {visible}")]
    Amended {
        /// The change.
        change: String,
        /// The recorded commit.
        commit: String,
        /// The visible commit.
        visible: String,
        /// The head of the stack being landed, when the change is below it
        /// (R3.10).
        head: Option<String>,
    },
    /// The destination is a remote bookmark.
    #[error("{0:?} is not a local bookmark; the landing service lands on local bookmarks")]
    DestinationNotLocal(String),
    /// The destination bookmark does not exist.
    #[error("no local bookmark {0:?}")]
    DestinationNotFound(String),
    /// The destination bookmark is conflicted.
    #[error("bookmark {0:?} is conflicted")]
    DestinationConflicted(String),
    /// The repository's queue is full.
    #[error("{MAX_QUEUED} changes are already queued in this repository")]
    QueueFull,
    /// The change's stack has more than [`land::MAX_STACK`] changes (R3.10).
    #[error("the stack has more than {max} changes")]
    StackTooLarge {
        /// Its changes, counted to one past the bound.
        size: usize,
        /// The bound.
        max: usize,
    },
    /// A change of the stack is proposed for another destination (R3.10).
    #[error(
        "change {change}, below it in its stack, is proposed for {destination}, not {expected}"
    )]
    StackDestinationMismatch {
        /// The member.
        change: String,
        /// Its destination.
        destination: String,
        /// The head's.
        expected: String,
    },
    /// A wait over [`MAX_WAIT`].
    #[error("wait_ms is at most {}", MAX_WAIT.as_millis())]
    InvalidWait,
    /// No landing has the id, or the change has none.
    #[error("no landing {0}")]
    NotFound(String),
    /// No batch has the id, or the change was in none (R3.14).
    #[error("no batch {0}")]
    BatchNotFound(String),
    /// Both or neither of an id and a change.
    #[error("name a landing id or a change, not both")]
    Selector,
    /// The revision could not be resolved.
    #[error("resolve the revision")]
    Revision(#[from] RevisionError),
    /// The commit could not be checked.
    #[error("check the commit")]
    Rewrite(#[from] RewriteError),
    /// A lifecycle record failed.
    #[error(transparent)]
    Lifecycle(#[from] LifecycleError),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// The repository thread failed.
    #[error(transparent)]
    Thread(#[from] RepoThreadError),
    /// The repository is not open, or a write failed.
    #[error(transparent)]
    Manager(#[from] ManagerError),
    /// The repository could not be read.
    #[error("read the repository")]
    Open(#[from] reef_lib::jj::OpenError),
    /// The policy input could not be read.
    #[error("read the policy input: {0}")]
    Input(String),
    /// The destination pushes to a git remote (R3.17) and the daemon's
    /// `[identity]` is the default.
    #[error(
        "{destination} is pushed to a git remote and the daemon's identity is the default reef <{email}>"
    )]
    IdentityUnset {
        /// The destination.
        destination: String,
        /// The default email.
        email: String,
        /// The daemon's config file.
        config: std::path::PathBuf,
    },
}

impl Classify for LandingError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::DependencyLeftReview { .. }
            | Self::NotProposed { .. }
            | Self::Amended { .. }
            | Self::DestinationNotLocal(_)
            | Self::DestinationNotFound(_)
            | Self::DestinationConflicted(_)
            | Self::StackTooLarge { .. }
            | Self::StackDestinationMismatch { .. }
            | Self::IdentityUnset { .. } => ErrorKind::Conflict,
            Self::QueueFull => ErrorKind::Unavailable,
            Self::InvalidWait | Self::Selector => ErrorKind::InvalidArgument,
            Self::NotFound(_) | Self::BatchNotFound(_) => ErrorKind::NotFound,
            Self::Revision(error) => error.kind(),
            Self::Rewrite(error) => error.kind(),
            Self::Lifecycle(error) => error.kind(),
            Self::Db(error) => error.kind(),
            Self::Thread(error) => error.kind(),
            Self::Manager(error) => error.kind(),
            Self::Open(error) => error.kind(),
            Self::Input(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for LandingError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::NotProposed {
                change,
                state,
                head,
            } if *state == LifecycleState::Draft
                || *state == LifecycleState::Withdrawn
                || *state == LifecycleState::Declined =>
            {
                Some(match head {
                    Some(head) => Advice::run(
                        "A change below it in its stack is not proposed; proposing the head proposes the stack.",
                        ["reef", "propose", head.as_str()],
                    ),
                    None => Advice::run(
                        "Propose it first; proposing is the request to land.",
                        ["reef", "propose", change.as_str()],
                    ),
                })
            }
            Self::DependencyLeftReview {
                change,
                state,
                moves: (above, onto),
                ..
            } => Some(Advice::run(
                format!(
                    "Change {change} below it was {state}. Move the changes above it off it, or withdraw this one; proposing the stack again would not put {change} back."
                ),
                [
                    "reef",
                    "rebase",
                    above.as_str(),
                    "--source",
                    "--onto",
                    onto.as_str(),
                ],
            )),
            Self::NotProposed { change, state, .. } if *state == LifecycleState::Queued => {
                Some(Advice::run(
                    "It is already queued; follow its landing.",
                    ["reef", "landing", "show", change.as_str()],
                ))
            }
            Self::NotProposed { .. } => Some(Advice::hint(
                "A landed or superseded change is final; make a new change instead.",
            )),
            Self::Amended { change, head, .. } => Some(Advice::run(
                "The change moved since it was proposed; propose it again to record the new commit, then land it.",
                [
                    "reef",
                    "propose",
                    head.as_deref().unwrap_or(change.as_str()),
                ],
            )),
            Self::DestinationNotLocal(name) => {
                let local = name.split('@').next().unwrap_or(name);
                Some(Advice::hint(format!(
                    "Track it as a local bookmark (`jj bookmark track {name}`), then propose the change for `{local}` with `reef propose --to {local}`."
                )))
            }
            Self::DestinationNotFound(_) => Some(Advice::hint(
                "Propose the change again for an existing bookmark with `reef propose --to <bookmark>`.",
            )),
            Self::DestinationConflicted(name) => Some(Advice::hint(format!(
                "Resolve the bookmark first: `jj bookmark set {name} -r <commit>`."
            ))),
            Self::StackTooLarge { .. } => Some(Advice::hint(
                "Land the lower part of the stack first: `reef land <rev>` on a change lower down.",
            )),
            Self::StackDestinationMismatch { expected, .. } => Some(Advice::hint(format!(
                "Propose the stack for one destination: `reef propose <rev> --to {expected}` on the head."
            ))),
            Self::QueueFull => Some(Advice::run(
                "Wait for queued landings to finish.",
                ["reef", "landing", "list", "--outcome", "queued"],
            )),
            Self::InvalidWait => Some(Advice::hint(
                "Wait at most ten minutes; poll with `reef landing show`.",
            )),
            Self::NotFound(_) => Some(Advice::run(
                "List the repository's landings.",
                ["reef", "landing", "list"],
            )),
            Self::BatchNotFound(_) => Some(Advice::run(
                "List the repository's batches.",
                ["reef", "landing", "batches"],
            )),
            Self::IdentityUnset { config, .. } => Some(Advice::hint(format!(
                "Landed commits on this branch reach its git remote, so the daemon's identity must be yours: set [identity] name and email in {}, then restart the daemon (`reef daemon stop`; the next reef command starts it).",
                config.display()
            ))),
            _ => None,
        }
    }
}

impl LandingError {
    /// Its reason and metadata, as `crate::error` reports them.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            Self::DependencyLeftReview {
                change,
                state,
                transition,
                ..
            } => (
                reason::DEPENDENCY_LEFT_REVIEW,
                vec![
                    ("change_id", change.clone()),
                    ("state", state.as_str().to_owned()),
                    ("transition_id", transition.clone()),
                ],
            ),
            Self::NotProposed { change, state, .. } => (
                reason::NOT_PROPOSED,
                vec![
                    ("change_id", change.clone()),
                    ("state", state.as_str().to_owned()),
                ],
            ),
            Self::Amended {
                change,
                commit,
                visible,
                ..
            } => (
                reason::PROPOSAL_AMENDED,
                vec![
                    ("change_id", change.clone()),
                    ("commit_id", commit.clone()),
                    ("visible_commit_id", visible.clone()),
                ],
            ),
            Self::DestinationNotLocal(name) => (
                reason::DESTINATION_NOT_LOCAL,
                vec![("destination", name.clone())],
            ),
            Self::DestinationNotFound(name) => (
                reason::DESTINATION_NOT_FOUND,
                vec![("destination", name.clone())],
            ),
            Self::DestinationConflicted(name) => (
                reason::DESTINATION_CONFLICTED,
                vec![("destination", name.clone())],
            ),
            Self::QueueFull => (reason::QUEUE_FULL, vec![("max", MAX_QUEUED.to_string())]),
            Self::StackTooLarge { size, max } => (
                reason::STACK_TOO_LARGE,
                vec![("size", size.to_string()), ("max", max.to_string())],
            ),
            Self::StackDestinationMismatch {
                change,
                destination,
                ..
            } => (
                reason::STACK_DESTINATION_MISMATCH,
                vec![
                    ("change_id", change.clone()),
                    ("destination", destination.clone()),
                ],
            ),
            Self::IdentityUnset { destination, .. } => (
                reason::IDENTITY_UNSET,
                vec![("destination", destination.clone())],
            ),
            Self::InvalidWait => (
                reason::INVALID_WAIT,
                vec![("max", MAX_WAIT.as_millis().to_string())],
            ),
            Self::BatchNotFound(what) => (
                reason::BATCH_NOT_FOUND,
                vec![(
                    if what.starts_with("bat_") {
                        "id"
                    } else {
                        "change_id"
                    },
                    what.clone(),
                )],
            ),
            Self::NotFound(what) => (
                reason::LANDING_NOT_FOUND,
                vec![(
                    if what.starts_with("lnd_") {
                        "id"
                    } else {
                        "change_id"
                    },
                    what.clone(),
                )],
            ),
            _ => return None,
        })
    }
}

/// A client's request to land, resolved by the service.
#[derive(Clone, Debug)]
pub struct LandRequest {
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
    /// The commit the caller expects; empty for no check.
    pub expected_commit: String,
    /// How long to wait for the landing.
    pub wait: Duration,
    /// The declared actor, if any.
    pub actor: Option<Actor>,
    /// The call's agent, marked with its remote token.
    pub agent: Option<String>,
    /// The remote token, `(id, name)`.
    pub remote: Option<(String, String)>,
    /// The automatic service's evaluation key, when it is the one asking
    /// (R3.45): stored on the rows it queues.
    pub automatic_key: Option<String>,
}

/// What [`land_change`] did.
#[derive(Debug)]
pub struct LandOutcome {
    /// The landing as it was when the wait ended.
    pub landing: StoredLanding,
    /// The landings of the other changes of its stack, bottom first, as
    /// they were when the wait ended (R3.10).
    pub stack: Vec<StoredLanding>,
    /// The enqueue operation, when this call queued the change.
    pub enqueue: Option<OperationIdHex>,
    /// The change, as the request resolved it.
    pub change: Option<ChangeInfo>,
}

/// Who asked for a landing, as its row and transitions record it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Requester {
    pub(crate) agent: Option<String>,
    pub(crate) actor: Option<String>,
    pub(crate) identity_id: Option<String>,
    pub(crate) identity_name: Option<String>,
    pub(crate) session_id: Option<String>,
    pub(crate) remote: Option<(String, String)>,
    /// The agent as sent, still marked with its remote token, for
    /// `reef.agent`.
    pub(crate) marked: Option<String>,
}

/// Whether the policy a judgment ran says `[landing] push` (R3.17).
fn pushes(state: &AppState, policy: &PolicyUsed) -> bool {
    policy.bytes.as_deref().is_some_and(|bytes| {
        matches!(&*state.policy.load(bytes), crate::policy::Loaded::Policy(policy) if policy.landing().push.is_some())
    })
}

/// The policy bytes a judgment ran, and where they came from.
#[derive(Clone, Debug, Default)]
pub(crate) struct PolicyUsed {
    pub(crate) commit: String,
    pub(crate) blob: Option<String>,
    pub(crate) sha256: Option<String>,
    pub(crate) bytes: Option<Vec<u8>>,
}

/// A judgment that ran the policy.
#[derive(Clone, Debug)]
pub(crate) struct Judged {
    pub(crate) preview: Preview,
    pub(crate) tree: Option<String>,
    pub(crate) evaluation: Evaluation,
    pub(crate) policy: PolicyUsed,
    /// The change's touched paths (R3.14: `[landing] alone`).
    pub(crate) paths: Vec<String>,
}

/// What a judgment sees beyond the change itself (R3.14): the `landing`
/// input from a batch's chain instead of the change's own preview, the
/// `batch` input, and the tree whose evidence the input carries instead of
/// the change's own (a batch's tested tree).
///
/// For a change of a stack (R3.10, ADR 0078): `below`, the changes landing
/// before it, which its preview rebases first and among which its parents
/// may be; and `above`, the submitted trees of the changes above it in the
/// stack being landed, whose sign-offs count for it.
#[derive(Clone, Debug, Default)]
pub(crate) struct JudgeContext {
    pub(crate) landing: Option<reef_policy::input::Landing>,
    pub(crate) batch: reef_policy::Batch,
    pub(crate) evidence_tree: Option<String>,
    pub(crate) below: Vec<LandTarget>,
    pub(crate) above: Vec<String>,
    /// A policy file's bytes to judge with instead of the destination's
    /// (R3.45: `EvaluatePolicy` over a stack with `policy`).
    pub(crate) policy: Option<Vec<u8>>,
}

/// What judging a change found.
#[derive(Debug)]
pub(crate) enum Verdict {
    /// It may land.
    Allow(Box<Judged>),
    /// It may not, for `reason`.
    Block {
        reason: Reason,
        message: String,
        /// What was read before it stopped: the preview, and the
        /// evaluation for [`Reason::Policy`].
        preview: Option<Preview>,
        judged: Option<Box<Judged>>,
        /// The destination's head, when known.
        destination: Option<String>,
        /// For [`Reason::Destination`], what is wrong with it.
        problem: Option<DestinationProblem>,
    },
}

/// What is wrong with a landing's destination.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DestinationProblem {
    /// The bookmark does not exist.
    Missing,
    /// The bookmark is conflicted.
    Conflicted,
    /// The change's commit is already on it.
    AlreadyThere,
}

/// What the repository thread reads for a judgment.
struct Read {
    preview: Result<Preview, LandError>,
    change: Option<policy_input::ChangeRead>,
    policy: Option<policy_input::PolicyFile>,
}

/// Judges `target`: the preview at the destination's head, then the policy
/// there over the queued commit (R3.13), seeing what `context` says besides
/// the change (R3.10, R3.14). Reads only.
#[expect(clippy::too_many_lines, reason = "the reads, then the evaluation")]
pub(crate) async fn judge_with(
    state: &AppState,
    repo: RepoId,
    target: &LandTarget,
    context: &JudgeContext,
) -> Result<Verdict, LandingError> {
    let handle = state.manager.handle(repo)?;
    let read = {
        let target = target.clone();
        let below = context.below.clone();
        handle
            .call_async(move |head| {
                async move {
                    let repo = head.refresh().await?;
                    let preview = land::preview_above(&repo, &below, &target).await;
                    let Ok(found) = &preview else {
                        return Ok::<_, LandingError>(Read {
                            preview,
                            change: None,
                            policy: None,
                        });
                    };
                    let scope = RevsetScope {
                        workspace: None,
                        user_email: head.settings.user_email(),
                        root: &head.root,
                    };
                    let change =
                        policy_input::read_change(&repo, &scope, &target.commit, &|| false)
                            .await
                            .map_err(|status| LandingError::Input(status.message().to_owned()))?;
                    let tree = files::commit_tree(&repo, &found.destination)
                        .await
                        .map_err(|error| LandingError::Input(ErrorChain(&error).to_string()))?;
                    let policy = policy_input::policy_file(&tree)
                        .await
                        .map_err(|status| LandingError::Input(status.message().to_owned()))?;
                    Ok(Read {
                        preview,
                        change: Some(change),
                        policy,
                    })
                }
                .boxed_local()
            })
            .await??
    };
    let preview = match read.preview {
        Ok(preview) => preview,
        Err(error) => return refusal(error),
    };
    if preview.conflicted() {
        let message = format!(
            "rebasing onto {} conflicts in {} paths",
            target.bookmark, preview.conflicts_total
        );
        return Ok(Verdict::Block {
            reason: Reason::Conflicted,
            message,
            destination: Some(preview.destination.as_str().to_owned()),
            preview: Some(preview),
            judged: None,
            problem: None,
        });
    }
    if preview.empty {
        return Ok(Verdict::Block {
            reason: Reason::Empty,
            message: empty_message(&target.bookmark),
            destination: Some(preview.destination.as_str().to_owned()),
            preview: Some(preview),
            judged: None,
            problem: None,
        });
    }
    let Some(mut change) = read.change else {
        return Err(LandingError::Input("the change was not read".to_owned()));
    };
    LifecycleState::Queued
        .as_str()
        .clone_into(&mut change.change.state);
    change.change.resolutions = crate::assign::resolutions(
        state,
        repo,
        &change.change.id,
        &change.change.commit,
        change.change.conflicted,
    )
    .map_err(|error| LandingError::Input(ErrorChain(&error).to_string()))?;
    let paths = change.change.paths.clone();
    let evidence_tree = context.evidence_tree.as_ref().or(change.tree_id.as_ref());
    let mut evidence = match evidence_tree {
        Some(tree) => policy_input::read_evidence(state, repo, tree)
            .await
            .map_err(|status| LandingError::Input(status.message().to_owned()))?,
        None => policy_input::EvidenceRead {
            evidence: Vec::new(),
            ignored: 0,
            invalid: Vec::new(),
        },
    };
    let policy_file = match &context.policy {
        Some(bytes) => Some(policy_input::PolicyFile {
            blob_id: String::new(),
            bytes: Ok(bytes.clone()),
        }),
        None => read.policy,
    };
    if !context.above.is_empty() {
        let signoffs = signoffs_above(state, repo, policy_file.as_ref(), &context.above).await?;
        for found in signoffs {
            if !evidence.evidence.iter().any(|known| known.id == found.id) {
                evidence.evidence.push(found);
            }
        }
    }
    let (provenance, provenance_ignored) =
        policy_input::read_provenance(state, repo, &change.change.id)
            .await
            .map_err(|status| LandingError::Input(status.message().to_owned()))?;
    let tree = change.tree_id.clone();
    let landing = context
        .landing
        .clone()
        .unwrap_or_else(|| policy_input::landing_input(&target.bookmark, &preview));
    let batch = context.batch.clone();
    let turn = Arc::clone(&state.policy.turns)
        .acquire_owned()
        .await
        .map_err(|error| LandingError::Input(error.to_string()))?;
    let evaluated = {
        let policy_file = policy_file.clone();
        let policies = Arc::clone(&state.policy);
        let proposed = change.proposed.clone();
        tokio::task::spawn_blocking(move || {
            let _turn = turn;
            let input = reef_policy::Input {
                change: change.change,
                evidence: evidence.evidence,
                provenance,
                evidence_ignored: evidence.ignored,
                provenance_ignored,
                proposed_policy: proposed_policy(&policies, proposed.as_ref()),
                landing,
                batch,
            };
            let mut evaluation = evaluate_file(&policies, policy_file.as_ref(), &input);
            policy_input::reject_invalid(&mut evaluation, &evidence.invalid);
            evaluation
        })
        .await
        .map_err(|error| LandingError::Input(error.to_string()))?
    };
    let policy = PolicyUsed {
        commit: preview.destination.as_str().to_owned(),
        blob: policy_file.as_ref().map(|file| file.blob_id.clone()),
        sha256: Some(evaluated.sha256.clone()).filter(|sha| !sha.is_empty()),
        bytes: policy_file.and_then(|file| file.bytes.ok()),
    };
    let judged = Box::new(Judged {
        preview: preview.clone(),
        tree,
        evaluation: evaluated,
        policy,
        paths,
    });
    if judged.evaluation.decision == Decision::Allow {
        return Ok(Verdict::Allow(judged));
    }
    let message = policy_message(&judged.evaluation);
    Ok(Verdict::Block {
        reason: Reason::Policy,
        message,
        destination: Some(preview.destination.as_str().to_owned()),
        preview: Some(preview),
        judged: Some(judged),
        problem: None,
    })
}

/// The sign-offs on `trees`, the submitted trees of the changes above one
/// in its stack (R3.10): the verified evidence of a kind `policy` counts as
/// a sign-off. A reviewer reads a stack as one diff, so accepting a change
/// accepts the changes below it.
async fn signoffs_above(
    state: &AppState,
    repo: RepoId,
    policy: Option<&policy_input::PolicyFile>,
    trees: &[String],
) -> Result<Vec<reef_policy::input::Evidence>, LandingError> {
    use crate::policy::Loaded;
    let Some(Ok(bytes)) = policy.map(|file| &file.bytes) else {
        return Ok(Vec::new());
    };
    let loaded = state.policy.load(bytes);
    let Loaded::Policy(policy) = &*loaded else {
        return Ok(Vec::new());
    };
    let kinds = policy.signoff_kinds();
    let mut found = Vec::new();
    for tree in trees {
        let read = policy_input::read_evidence(state, repo, tree)
            .await
            .map_err(|status| LandingError::Input(status.message().to_owned()))?;
        found.extend(
            read.evidence
                .into_iter()
                .filter(|evidence| kinds.contains(evidence.kind.as_str())),
        );
    }
    Ok(found)
}

/// `proposed` as the input's `proposed_policy`, through `policies`' cache.
fn proposed_policy(
    policies: &crate::policy::Policies,
    proposed: Option<&policy_input::PolicyFile>,
) -> reef_policy::input::ProposedPolicy {
    use crate::policy::Loaded;
    match proposed {
        None => reef_policy::input::ProposedPolicy::default(),
        Some(file) => match &file.bytes {
            Ok(bytes) => reef_policy::input::ProposedPolicy {
                present: true,
                sha256: reef_policy::sha256_hex(bytes),
                diagnostics: match &*policies.load(bytes) {
                    Loaded::Policy(_) => Vec::new(),
                    Loaded::Invalid(diagnostics) => diagnostics.clone(),
                },
            },
            Err(problem) => reef_policy::input::ProposedPolicy {
                present: true,
                sha256: String::new(),
                diagnostics: vec![reef_policy::Diagnostic {
                    message: problem.clone(),
                    ..reef_policy::Diagnostic::default()
                }],
            },
        },
    }
}

/// The policy file `file` (none for no file) evaluated over `input`.
fn evaluate_file(
    policies: &crate::policy::Policies,
    file: Option<&policy_input::PolicyFile>,
    input: &reef_policy::Input,
) -> Evaluation {
    use crate::policy::Loaded;
    match file {
        None => Evaluation::no_policy(input),
        Some(file) => match &file.bytes {
            Ok(bytes) => match &*policies.load(bytes) {
                Loaded::Policy(policy) => policy.evaluate(input),
                Loaded::Invalid(diagnostics) => {
                    Evaluation::invalid(bytes, diagnostics.clone(), input)
                }
            },
            Err(problem) => {
                let mut evaluation = Evaluation::invalid(
                    &[],
                    vec![reef_policy::Diagnostic {
                        message: problem.clone(),
                        ..reef_policy::Diagnostic::default()
                    }],
                    input,
                );
                evaluation.sha256.clear();
                evaluation
            }
        },
    }
}

/// A one-line summary of why `evaluation` is not `ALLOW`.
pub(crate) fn policy_message(evaluation: &Evaluation) -> String {
    match evaluation.decision {
        Decision::Allow => String::new(),
        Decision::NoPolicy => {
            "the destination has no reef-policy.toml, so nothing lands automatically".to_owned()
        }
        Decision::InvalidPolicy => {
            "the destination's reef-policy.toml does not load, so nothing lands".to_owned()
        }
        Decision::Block => {
            let names: Vec<&str> = evaluation
                .rules
                .iter()
                .filter(|rule| {
                    matches!(
                        rule.status,
                        reef_policy::Status::Unsatisfied | reef_policy::Status::Error
                    )
                })
                .map(|rule| rule.name.as_str())
                .collect();
            format!("blocked by {}", names.join(", "))
        }
    }
}

/// A refused preview as a verdict, or the request error it is.
fn refusal(error: LandError) -> Result<Verdict, LandingError> {
    let block = |reason, message: String| {
        Ok(Verdict::Block {
            reason,
            message,
            preview: None,
            judged: None,
            destination: None,
            problem: None,
        })
    };
    let destination = |problem, message: String| {
        Ok(Verdict::Block {
            reason: Reason::Destination,
            message,
            preview: None,
            judged: None,
            destination: None,
            problem: Some(problem),
        })
    };
    match error {
        LandError::DestinationNotFound(name) => destination(
            DestinationProblem::Missing,
            format!("no local bookmark {name:?}"),
        ),
        LandError::DestinationConflicted(name) => destination(
            DestinationProblem::Conflicted,
            format!("bookmark {name:?} is conflicted"),
        ),
        error @ LandError::AlreadyOnDestination(_) => {
            destination(DestinationProblem::AlreadyThere, error.to_string())
        }
        error @ LandError::Amended { .. } => block(Reason::Amended, error.to_string()),
        error @ (LandError::UnlandedParents { .. } | LandError::ParentExcluded { .. }) => {
            block(Reason::UnlandedParents, error.to_string())
        }
        error @ (LandError::Conflicted(_) | LandError::RebaseConflicts) => {
            block(Reason::Conflicted, error.to_string())
        }
        error @ LandError::DestinationMoved { .. } => block(Reason::Busy, error.to_string()),
        error @ LandError::Empty(_) => block(Reason::Empty, error.to_string()),
        error => Err(LandingError::Input(ErrorChain(&error).to_string())),
    }
}

/// Why a change that would land empty is returned (R3.16).
pub(crate) fn empty_message(bookmark: &str) -> String {
    format!("its changes are already on {bookmark}: there is nothing left to land")
}

/// The ids of the evidence each satisfied rule of `evaluation` used.
pub(crate) fn pins(evaluation: &Evaluation) -> Vec<String> {
    let mut ids: Vec<String> = evaluation
        .rules
        .iter()
        .filter(|rule| rule.status == reef_policy::Status::Satisfied)
        .flat_map(|rule| rule.evidence_ids.iter().cloned())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// `{"paths": [...], "total": n}`.
fn paths_json(paths: &[String], total: usize) -> String {
    serde_json::json!({ "paths": paths, "total": total }).to_string()
}

/// A landing change row for `target` with what `verdict` found.
pub(crate) fn change_row(
    target: &LandTarget,
    queued_tree: Option<String>,
    preview: Option<&Preview>,
    judged: Option<&Judged>,
) -> LandingChangeRow {
    let preview = judged.map(|judged| &judged.preview).or(preview);
    let evaluation =
        judged.map(|judged| serde_json::to_string(&judged.evaluation).unwrap_or_default());
    LandingChangeRow {
        position: 0,
        change_id: target.change.as_str().to_owned(),
        queued_commit: target.commit.as_str().to_owned(),
        queued_tree: judged
            .and_then(|judged| judged.tree.clone())
            .or(queued_tree),
        landed_commit: None,
        landed_tree: None,
        rebased: preview.is_some_and(|preview| preview.rebased),
        overlap: preview.map(|preview| paths_json(&preview.overlap, preview.overlap_total)),
        conflicts: preview
            .filter(|preview| preview.conflicted())
            .map(|preview| paths_json(&preview.conflicts, preview.conflicts_total)),
        decision: judged.map(|judged| decision_text(judged.evaluation.decision).to_owned()),
        evaluation_sha256: evaluation
            .as_deref()
            .map(|json| reef_policy::sha256_hex(json.as_bytes())),
        evaluation,
        tested_tree: None,
        tested_commit: None,
        tested_evaluation: None,
        batch_position: None,
        summarized: false,
    }
}

/// A decision's text.
pub(crate) fn decision_text(decision: Decision) -> &'static str {
    match decision {
        Decision::Allow => "allow",
        Decision::Block => "block",
        Decision::NoPolicy => "no_policy",
        Decision::InvalidPolicy => "invalid_policy",
    }
}

/// Who `request` is, as a landing records it.
fn requester(state: &AppState, request: &LandRequest) -> Requester {
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
    let (identity_id, identity_name, session_id) =
        crate::lifecycle::identity_for(state, request.workspace.as_ref(), request.remote.as_ref());
    let remote = request
        .remote
        .clone()
        .map(|(id, name)| (id, remote_name.unwrap_or(name)));
    Requester {
        agent,
        actor: Some(actor.as_str().to_owned()),
        identity_id,
        identity_name,
        session_id,
        remote,
        marked: request.agent.clone().or_else(|| {
            request
                .workspace
                .as_ref()
                .map(|record| record.owner.clone())
        }),
    }
}

/// A new landing row for `destination`, asked for by `who`.
pub(crate) fn new_row(repo: RepoId, destination: &str, who: &Requester) -> LandingRow {
    LandingRow {
        repo_id: repo,
        id: crate::identity::new_id("lnd_"),
        destination: destination.to_owned(),
        destination_before: None,
        destination_after: None,
        outcome: Outcome::Queued.as_str().to_owned(),
        reason: None,
        message: None,
        policy_commit: None,
        policy_blob: None,
        policy_sha256: None,
        attempts: 1,
        enqueue_operation_id: None,
        operation_id: None,
        operation_collected: false,
        agent: who.agent.clone(),
        actor: who.actor.clone(),
        identity_name: who.identity_name.clone(),
        remote_token: who.remote.as_ref().map(|(id, _)| id.clone()),
        remote_token_name: who.remote.as_ref().map(|(_, name)| name.clone()),
        created_at: Timestamp::now(),
        finished_at: None,
        batch_id: None,
        history: None,
        pushed_at: None,
        push_operation_id: None,
        stack: None,
        stack_position: None,
        automatic_key: None,
    }
}

/// A daemon transition row of `change` at `commit`, from `from` to `to`,
/// cause `landing`.
#[expect(clippy::too_many_arguments, reason = "a row's columns")]
pub(crate) fn transition_row(
    repo: RepoId,
    change: &str,
    commit: &str,
    from: LifecycleState,
    to: LifecycleState,
    round: u32,
    destination: &str,
    reason: Option<String>,
    who: &Requester,
    actor: Actor,
) -> TransitionRow {
    TransitionRow {
        repo_id: repo,
        id: crate::identity::new_id("trn_"),
        seq: None,
        committed: false,
        change_id: change.to_owned(),
        commit_id: commit.to_owned(),
        from_state: from.as_str().to_owned(),
        to_state: to.as_str().to_owned(),
        cause: TransitionCause::Landing.as_str().to_owned(),
        round,
        reason,
        superseded_by: None,
        destination: Some(destination.to_owned()),
        undoes_operation_id: None,
        undoes_transition_id: None,
        operation_id: None,
        operation_collected: false,
        agent: who.agent.clone(),
        actor: actor.as_str().to_owned(),
        identity_id: who.identity_id.clone(),
        identity_name: who.identity_name.clone(),
        session_id: who.session_id.clone(),
        remote_token: who.remote.as_ref().map(|(id, _)| id.clone()),
        remote_token_name: who.remote.as_ref().map(|(_, name)| name.clone()),
        created_at: Timestamp::now(),
        recorded_at: None,
        dependency_change_id: None,
        dependency_event: None,
    }
}

/// The request's change on the repository thread: resolved, checked
/// against `expected_commit`, and described.
async fn resolve(
    head: &mut crate::repo::RepoHead,
    request: &LandRequest,
) -> Result<ChangeInfo, LandingError> {
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
    let describer = Describer::new(&repo, &scope).map_err(LandingError::Revision)?;
    Ok(describer.describe(&resolved.commit_id).await?)
}

/// What [`land_change`] answers.
#[derive(Debug)]
pub enum Landed {
    /// The gate judged it: the landing, as the wait left it.
    Judged(Box<LandOutcome>),
    /// It still waits for evidence the automatic service asked for (R3.45):
    /// nothing was judged or recorded.
    Waiting {
        /// Its automatic status.
        status: Box<AutomaticStatus>,
        /// The change, as the request resolved it.
        change: Box<ChangeInfo>,
    },
}

/// Asks for a landing; see the module docs. A change waiting for evidence
/// the automatic service asked executors for is judged once that evidence
/// is in, within the wait (R3.45). Runs as its own task, so a caller that
/// stops waiting cannot cut it between its enqueue's operation and its row.
pub async fn land_change(
    state: &Arc<AppState>,
    request: LandRequest,
) -> Result<Landed, LandingError> {
    if request.wait > MAX_WAIT {
        return Err(LandingError::InvalidWait);
    }
    let started = Instant::now();
    let wait = request.wait;
    // A dependent whose dependency was just rewritten is carried before the
    // gate reads it (R3.11), so it is not refused as amended.
    if let Err(error) = crate::lifecycle::carry::carry(state, request.repo, None).await {
        tracing::warn!(error = %ErrorChain(&error), "cannot carry proposals before a landing; the gate reads them as they are");
    }
    let info = {
        let request = request.clone();
        state
            .manager
            .handle(request.repo)?
            .call_async(move |head| async move { resolve(head, &request).await }.boxed_local())
            .await??
    };
    if let Some(status) = automatic::wait_for_evidence(
        state,
        request.repo,
        info.change_id.as_str(),
        started + wait,
        !wait.is_zero(),
    )
    .await
    {
        return Ok(Landed::Waiting {
            status: Box::new(status),
            change: Box::new(info),
        });
    }
    let task_state = Arc::clone(state);
    let repo = request.repo;
    let span = tracing::Span::current();
    let mut outcome = tokio::spawn(
        async move {
            let outcome = gate(&task_state, request).await?;
            // Woken here, so a caller that goes away cannot leave a queued
            // change with no worker.
            if std::iter::once(&outcome.landing)
                .chain(&outcome.stack)
                .any(|landing| landing.row.outcome == Outcome::Queued.as_str())
            {
                worker::wake(&task_state, outcome.landing.row.repo_id);
            }
            Ok::<_, LandingError>(outcome)
        }
        .instrument_with(span),
    )
    .await
    .map_err(|_| LandingError::Thread(RepoThreadError::Panicked { id: repo }))??;
    let deadline = started + wait;
    for member in &mut outcome.stack {
        if member.row.outcome == Outcome::Queued.as_str() {
            *member = wait_for(state, repo, &member.row.id, deadline).await?;
        }
    }
    if outcome.landing.row.outcome == Outcome::Queued.as_str() {
        outcome.landing = wait_for(state, repo, &outcome.landing.row.id, deadline).await?;
    }
    Ok(Landed::Judged(Box::new(outcome)))
}

/// Waits until landing `id` is no longer queued, or `deadline`.
async fn wait_for(
    state: &AppState,
    repo: RepoId,
    id: &str,
    deadline: Instant,
) -> Result<StoredLanding, LandingError> {
    loop {
        let finished = state.landings.finished.notified();
        let landing = state
            .db
            .landing(repo, id)?
            .ok_or_else(|| LandingError::NotFound(id.to_owned()))?;
        let now = Instant::now();
        if landing.row.outcome != Outcome::Queued.as_str() || now >= deadline {
            return Ok(landing);
        }
        let _ = tokio::time::timeout(deadline - now, finished).await;
    }
}

/// One change of the stack a request lands (R3.10), before judging.
pub(crate) struct Slot {
    pub(crate) target: LandTarget,
    /// Its submitted tree.
    pub(crate) tree: Option<String>,
    /// Its commit's parents.
    pub(crate) parents: Vec<CommitIdHex>,
    /// Its open landing, when an earlier request queued it.
    pub(crate) open: Option<StoredLanding>,
}

/// What the gate decided for one change of the stack.
pub(crate) enum Decided {
    /// Queued by an earlier request.
    Open(Box<StoredLanding>),
    /// It passed: it is queued now, with this row.
    Pass {
        target: LandTarget,
        row: Box<(LandingRow, Vec<LandingChangeRow>)>,
        judged: Box<Judged>,
    },
    /// It is refused, with this row (`None` until written, for a change
    /// landed alone, whose row may count one more attempt on an earlier
    /// one).
    Block {
        target: LandTarget,
        row: Box<(LandingRow, Vec<LandingChangeRow>)>,
        /// What was judged, when the policy ran.
        judged: Option<Box<Judged>>,
    },
}

/// Whether `paths` touch a policy file, which lands on its own (R3.14).
fn touches_policy(paths: &[String]) -> bool {
    paths.iter().any(|path| {
        reef_policy::POLICY_FILES
            .iter()
            .any(|file| path.eq_ignore_ascii_case(file))
    })
}

/// For each of `slots`, the indexes of the slots that are its ancestors,
/// from the parent edges (R3.10): which changes a sign-off reaches.
fn ancestors_of(slots: &[Slot]) -> Vec<HashSet<usize>> {
    let index: HashMap<&CommitIdHex, usize> = slots
        .iter()
        .enumerate()
        .map(|(at, slot)| (&slot.target.commit, at))
        .collect();
    let mut ancestors: Vec<HashSet<usize>> = Vec::with_capacity(slots.len());
    // Parents first, so each parent's set is complete when read.
    for slot in slots {
        let mut mine = HashSet::new();
        for parent in &slot.parents {
            if let Some(&at) = index.get(parent) {
                mine.insert(at);
                if let Some(theirs) = ancestors.get(at) {
                    mine.extend(theirs.iter().copied());
                }
            }
        }
        ancestors.push(mine);
    }
    ancestors
}

/// What the gate judged for a request, before it writes anything (R3.45:
/// the automatic service acts on the same judgment).
pub(crate) struct Judgment {
    /// The change the request named, the stack's head.
    pub(crate) info: ChangeInfo,
    /// Its proposal's destination.
    pub(crate) destination: String,
    /// Whether the stack has more than the head.
    pub(crate) stacked: bool,
    /// Each change of the stack, bottom first.
    pub(crate) decided: Vec<Decided>,
    /// Whether the policy that judged them pushes (R3.17).
    pub(crate) pushing: bool,
    who: Requester,
}

/// What [`decide`] found.
pub(crate) enum Gate {
    /// The change is queued already: the answer, as the gate gives it.
    Queued(Box<LandOutcome>),
    /// The judgment.
    Judged(Box<Judgment>),
}

/// The gate's checks and judgment, with no write: the change must be
/// proposed at its visible commit, and its stack below it (R3.10) proposed
/// for the same destination; each is judged in order, rebased onto the ones
/// below ([`judge_slots`]).
pub(crate) async fn decide(state: &AppState, request: &LandRequest) -> Result<Gate, LandingError> {
    let repo = request.repo;
    let handle = state.manager.handle(repo)?;
    let info = {
        let request = request.clone();
        handle
            .call_async(move |head| async move { resolve(head, &request).await }.boxed_local())
            .await??
    };
    let change = info.change_id.as_str().to_owned();
    let commit = info.commit_id.clone();
    let row = state.db.lifecycle_row(repo, &change)?;
    let current = row
        .as_ref()
        .and_then(|row| row.state.parse().ok())
        .unwrap_or(LifecycleState::Draft);
    if current == LifecycleState::Queued
        && let Some(open) = state.db.open_landing_of(repo, &change)?
    {
        return Ok(Gate::Queued(Box::new(queued_outcome(
            state, repo, open, info,
        )?)));
    }
    let Some(row) = row.filter(|_| current == LifecycleState::Proposed) else {
        return Err(LandingError::NotProposed {
            change,
            state: current,
            head: None,
        });
    };
    if row.commit_id != commit.as_str() {
        return Err(LandingError::Amended {
            change,
            commit: row.commit_id,
            visible: commit.as_str().to_owned(),
            head: None,
        });
    }
    let destination = row.destination.clone().unwrap_or_default();
    if destination.contains('@') {
        return Err(LandingError::DestinationNotLocal(destination));
    }
    let slots = stack_slots(state, repo, &info, &destination).await?;
    let who = requester(state, request);
    let (decided, pushing) = judge_slots(
        state,
        repo,
        &slots,
        &destination,
        &who,
        &JudgeContext::default(),
    )
    .await?;
    Ok(Gate::Judged(Box::new(Judgment {
        info,
        stacked: slots.len() > 1,
        destination,
        decided,
        pushing,
        who,
    })))
}

/// Judges `slots`, a stack bottom first, as the gate does (R3.10): each
/// rebased onto the ones below, sign-offs from the changes above that
/// descend from it counted, the first refusal stopping the prefix and the
/// changes above it `unlanded_parents`, a policy-file change ending the
/// prefix. `context` adds what every judgment sees (R3.45: a policy file's
/// bytes for `EvaluatePolicy`). Reads only. Returns the decisions, with
/// rows not yet written, and whether the policy pushes.
#[expect(clippy::too_many_lines, reason = "the gate's judgment in order")]
pub(crate) async fn judge_slots(
    state: &AppState,
    repo: RepoId,
    slots: &[Slot],
    destination: &str,
    who: &Requester,
    context: &JudgeContext,
) -> Result<(Vec<Decided>, bool), LandingError> {
    let base_context = context;
    let change = slots
        .last()
        .map(|slot| slot.target.change.as_str().to_owned())
        .unwrap_or_default();
    let ancestors = ancestors_of(slots);
    let stacked = slots.len() > 1;
    let head_id = crate::identity::new_id("lnd_");
    let base = Timestamp::now();

    // Judge in order; the first refusal stops the prefix.
    let mut decided = Vec::with_capacity(slots.len());
    let mut below: Vec<LandTarget> = Vec::new();
    let mut stop: Option<String> = None;
    let mut pushing = false;
    for (index, slot) in slots.iter().enumerate() {
        let short: String = slot.target.change.as_str().chars().take(12).collect();
        let context = JudgeContext {
            below: below.clone(),
            // Only the changes above it that have it as an ancestor: a
            // reviewer of those read its diff.
            above: slots
                .iter()
                .enumerate()
                .filter(|(at, _)| ancestors[*at].contains(&index))
                .filter_map(|(_, above)| above.tree.clone())
                .collect(),
            ..base_context.clone()
        };
        let mut row = new_row(repo, destination, who);
        row.created_at = Timestamp(base.0 + i64::try_from(index).unwrap_or(0));
        if index + 1 == slots.len() {
            row.id.clone_from(&head_id);
        }
        if stacked {
            row.stack = Some(head_id.clone());
            row.stack_position = Some(u32::try_from(index).unwrap_or(u32::MAX));
        }
        if let Some(open) = &slot.open {
            // Queued by an earlier request. A change to a policy file still
            // lands on its own (S-210): the changes above it wait.
            if stop.is_none()
                && index + 1 < slots.len()
                && let Some(paths) = paths_of(state, repo, &slot.target, &context).await?
                && touches_policy(&paths)
            {
                stop = Some(format!(
                    "change {short} below it changes a policy file and lands on its own first"
                ));
            }
            below.push(slot.target.clone());
            decided.push(Decided::Open(Box::new(open.clone())));
            continue;
        }
        if let Some(why) = &stop {
            Outcome::Blocked.as_str().clone_into(&mut row.outcome);
            row.reason = Some(Reason::UnlandedParents.as_str().to_owned());
            row.message = Some(why.clone());
            row.finished_at = Some(row.created_at);
            let change = change_row(&slot.target, slot.tree.clone(), None, None);
            decided.push(Decided::Block {
                target: slot.target.clone(),
                row: Box::new((row, vec![change])),
                judged: None,
            });
            continue;
        }
        match judge_with(state, repo, &slot.target, &context).await? {
            Verdict::Block {
                problem: Some(DestinationProblem::Missing),
                ..
            } => return Err(LandingError::DestinationNotFound(destination.to_owned())),
            Verdict::Block {
                problem: Some(DestinationProblem::Conflicted),
                ..
            } => return Err(LandingError::DestinationConflicted(destination.to_owned())),
            Verdict::Block {
                reason: Reason::Amended,
                ..
            } => {
                // The change moved between the gate's read and its preview.
                return Err(LandingError::Amended {
                    change: slot.target.change.as_str().to_owned(),
                    commit: slot.target.commit.as_str().to_owned(),
                    visible: "(moved during the request)".to_owned(),
                    head: stacked.then(|| change.clone()),
                });
            }
            Verdict::Block {
                reason,
                message,
                preview,
                judged,
                destination: head,
                ..
            } => {
                stop = Some(format!(
                    "change {short} below it was blocked: {}",
                    reason.as_str()
                ));
                blocked_row(&mut row, (reason, message), judged.as_deref(), head);
                let change = change_row(
                    &slot.target,
                    slot.tree.clone(),
                    preview.as_ref(),
                    judged.as_deref(),
                );
                decided.push(Decided::Block {
                    target: slot.target.clone(),
                    row: Box::new((row, vec![change])),
                    judged,
                });
            }
            Verdict::Allow(judged) => {
                let policy_change = touches_policy(&judged.paths);
                if policy_change && !below.is_empty() {
                    stop = Some(format!(
                        "change {short} below it changes a policy file and lands on its own"
                    ));
                    let head = Some(judged.preview.destination.as_str().to_owned());
                    blocked_row(
                        &mut row,
                        (
                            Reason::Alone,
                            "it changes reef-policy.toml or reef-checks.toml, which lands on its own: land the changes below it first".to_owned(),
                        ),
                        Some(&judged),
                        head,
                    );
                    let change = change_row(&slot.target, slot.tree.clone(), None, Some(&judged));
                    decided.push(Decided::Block {
                        target: slot.target.clone(),
                        row: Box::new((row, vec![change])),
                        judged: Some(judged),
                    });
                    continue;
                }
                if policy_change {
                    stop = Some(format!(
                        "change {short} below it changes a policy file and lands on its own first"
                    ));
                }
                pushing |= pushes(state, &judged.policy);
                row.destination_before = Some(judged.preview.destination.as_str().to_owned());
                row.policy_commit = Some(judged.policy.commit.clone());
                row.policy_blob.clone_from(&judged.policy.blob);
                row.policy_sha256.clone_from(&judged.policy.sha256);
                let change = change_row(&slot.target, slot.tree.clone(), None, Some(&judged));
                below.push(slot.target.clone());
                decided.push(Decided::Pass {
                    target: slot.target.clone(),
                    row: Box::new((row, vec![change])),
                    judged,
                });
            }
        }
    }
    Ok((decided, pushing))
}

/// The gate: [`decide`], then its writes: a refusal of a change landed alone
/// is a blocked row, and a stack's passing prefix is queued in one operation
/// with the rest blocked.
async fn gate(state: &AppState, request: LandRequest) -> Result<LandOutcome, LandingError> {
    match decide(state, &request).await? {
        Gate::Queued(outcome) => Ok(*outcome),
        Gate::Judged(judgment) => write(state, request, *judgment).await,
    }
}

/// The gate's writes for `judgment` (R3.45: also the automatic service's).
pub(crate) async fn write(
    state: &AppState,
    request: LandRequest,
    judgment: Judgment,
) -> Result<LandOutcome, LandingError> {
    let Judgment {
        info,
        destination,
        stacked,
        mut decided,
        pushing,
        who,
    } = judgment;
    let repo = request.repo;
    if let Some(key) = &request.automatic_key {
        for decision in &mut decided {
            if let Decided::Pass { row, .. } = decision {
                row.0.automatic_key = Some(key.clone());
            }
        }
    }
    // A destination pushed to a git remote takes no landing while the
    // daemon's identity is the default (R3.17): it would reach the host.
    if pushing && state.config.identity_is_default() {
        return Err(LandingError::IdentityUnset {
            destination,
            email: state.config.identity.email.clone(),
            config: state.config.paths.config_file.clone(),
        });
    }

    // A change landed alone and refused may count one more attempt on its
    // latest identical row (S-125); a stack's rows are the request's own.
    if !stacked && let Some(Decided::Block { target, row, .. }) = decided.last() {
        let (row, changes) = &**row;
        let landing = blocked(state, repo, target, row, &changes[0])?;
        return Ok(LandOutcome {
            landing,
            stack: Vec::new(),
            enqueue: None,
            change: Some(info),
        });
    }
    let (enqueue, mut written) = enqueue(state, &request, &who, &destination, decided).await?;
    let landing = written
        .pop()
        .ok_or_else(|| LandingError::Input("no change to land".to_owned()))?;
    Ok(LandOutcome {
        landing,
        stack: written,
        enqueue,
        change: Some(info),
    })
}

/// `row` as a landing the gate refused for `reason`.
fn blocked_row(
    row: &mut LandingRow,
    (reason, message): (Reason, String),
    judged: Option<&Judged>,
    head: Option<String>,
) {
    Outcome::Blocked.as_str().clone_into(&mut row.outcome);
    row.reason = Some(reason.as_str().to_owned());
    row.message = Some(message);
    row.destination_before = head;
    if let Some(judged) = judged {
        row.policy_commit = Some(judged.policy.commit.clone());
        row.policy_blob.clone_from(&judged.policy.blob);
        row.policy_sha256.clone_from(&judged.policy.sha256);
    }
    row.finished_at = Some(row.created_at);
}

/// The paths an earlier request's queued change touches, judged as the
/// gate judges (R3.10: whether it changes a policy file); `None` when the
/// judgment stopped before reading them.
async fn paths_of(
    state: &AppState,
    repo: RepoId,
    target: &LandTarget,
    context: &JudgeContext,
) -> Result<Option<Vec<String>>, LandingError> {
    Ok(match judge_with(state, repo, target, context).await? {
        Verdict::Allow(judged)
        | Verdict::Block {
            judged: Some(judged),
            ..
        } => Some(judged.paths),
        Verdict::Block { .. } => None,
    })
}

/// The answer for a change already queued: its open landing and the other
/// landings of the request that queued it.
fn queued_outcome(
    state: &AppState,
    repo: RepoId,
    open: StoredLanding,
    info: ChangeInfo,
) -> Result<LandOutcome, LandingError> {
    let stack = match &open.row.stack {
        Some(stack) => state
            .db
            .landings_of_stack(repo, stack)?
            .into_iter()
            .filter(|member| member.row.id != open.row.id)
            .collect(),
        None => Vec::new(),
    };
    Ok(LandOutcome {
        landing: open,
        stack,
        enqueue: None,
        change: Some(info),
    })
}

/// `info`'s stack onto `destination`'s head (R3.10), read on the
/// repository thread.
async fn read_stack(
    state: &AppState,
    repo: RepoId,
    info: &ChangeInfo,
    destination: &str,
) -> Result<Vec<land::StackMember>, LandingError> {
    let handle = state.manager.handle(repo)?;
    let found = {
        let bookmark = destination.to_owned();
        let head = info.commit_id.clone();
        handle
            .call_async(move |repo_head| {
                async move {
                    let repo = repo_head.refresh().await?;
                    let found = match land::destination_head(&repo, &bookmark) {
                        Ok(onto) => land::stack(&repo, &head, &onto).await,
                        Err(error) => Err(error),
                    };
                    Ok::<_, LandingError>(found)
                }
                .boxed_local()
            })
            .await??
    };
    match found {
        Ok(members) => Ok(members),
        Err(LandError::DestinationNotFound(_)) => {
            Err(LandingError::DestinationNotFound(destination.to_owned()))
        }
        Err(LandError::DestinationConflicted(_)) => {
            Err(LandingError::DestinationConflicted(destination.to_owned()))
        }
        Err(LandError::StackTooLarge { size, max }) => {
            Err(LandingError::StackTooLarge { size, max })
        }
        Err(error) => Err(LandingError::Input(ErrorChain(&error).to_string())),
    }
}

/// `DEPENDENCY_LEFT_REVIEW` for `change` of `members`, which left review
/// (R3.11), with the `reef rebase` that moves the changes above it off it.
fn left_review(
    app: &AppState,
    repo: RepoId,
    members: &[land::StackMember],
    (change, state, row): (String, LifecycleState, &crate::db::LifecycleRow),
    destination: &str,
) -> LandingError {
    let links: Vec<crate::lifecycle::dependents::Link> = members
        .iter()
        .map(|member| crate::lifecycle::dependents::Link {
            change: member.change.to_string(),
            commit: member.commit.to_string(),
            parents: member.parents.iter().map(ToString::to_string).collect(),
        })
        .collect();
    let at = links
        .iter()
        .position(|link| link.change == change)
        .unwrap_or_default();
    LandingError::DependencyLeftReview {
        moves: crate::lifecycle::dependents::move_off(&links, at, destination, |link| {
            app.db
                .lifecycle_row(repo, &link.change)
                .ok()
                .flatten()
                .is_some_and(|row| {
                    crate::lifecycle::dependents::left_review(&row, &link.commit).is_some()
                })
        }),
        change,
        state,
        transition: row.transition_id.clone(),
    }
}

/// The changes `info`'s stack lands onto `destination` (R3.10), bottom
/// first, the head last: each below the head must be proposed for the same
/// destination at its visible commit, or queued at it.
async fn stack_slots(
    state: &AppState,
    repo: RepoId,
    info: &ChangeInfo,
    destination: &str,
) -> Result<Vec<Slot>, LandingError> {
    let members = read_stack(state, repo, info, destination).await?;
    let target = |change: &ChangeIdHex, commit: &CommitIdHex| LandTarget {
        change: change.clone(),
        commit: commit.clone(),
        bookmark: destination.to_owned(),
    };
    let head = Some(info.change_id.as_str().to_owned());
    let mut slots = Vec::with_capacity(members.len().max(1));
    for member in members
        .iter()
        .filter(|member| member.commit != info.commit_id)
    {
        let change = member.change.as_str().to_owned();
        let row = state.db.lifecycle_row(repo, &change)?;
        let current = row
            .as_ref()
            .and_then(|row| row.state.parse().ok())
            .unwrap_or(LifecycleState::Draft);
        let amended = |commit: String| LandingError::Amended {
            change: change.clone(),
            commit,
            visible: member.commit.as_str().to_owned(),
            head: head.clone(),
        };
        let open = match (current, row) {
            (LifecycleState::Queued, _) => match state.db.open_landing_of(repo, &change)? {
                Some(open) => {
                    let queued = open
                        .changes
                        .first()
                        .map(|first| first.queued_commit.clone())
                        .unwrap_or_default();
                    if queued != member.commit.as_str() {
                        return Err(amended(queued));
                    }
                    Some(open)
                }
                None => {
                    return Err(LandingError::NotProposed {
                        change,
                        state: current,
                        head,
                    });
                }
            },
            (LifecycleState::Proposed, Some(row)) => {
                if row.commit_id != member.commit.as_str() {
                    return Err(amended(row.commit_id));
                }
                let proposed_for = row.destination.unwrap_or_default();
                if proposed_for != destination {
                    return Err(LandingError::StackDestinationMismatch {
                        change,
                        destination: proposed_for,
                        expected: destination.to_owned(),
                    });
                }
                None
            }
            (_, Some(row))
                if crate::lifecycle::dependents::left_review(&row, member.commit.as_str())
                    .is_some() =>
            {
                return Err(left_review(
                    state,
                    repo,
                    &members,
                    (change, current, &row),
                    destination,
                ));
            }
            _ => {
                return Err(LandingError::NotProposed {
                    change,
                    state: current,
                    head,
                });
            }
        };
        slots.push(Slot {
            target: target(&member.change, &member.commit),
            tree: member.tree.clone(),
            parents: member.parents.clone(),
            open,
        });
    }
    let parents = members
        .iter()
        .find(|member| member.commit == info.commit_id)
        .map(|member| member.parents.clone())
        .unwrap_or_default();
    slots.push(Slot {
        target: target(&info.change_id, &info.commit_id),
        tree: info.tree_id.clone(),
        parents,
        open: None,
    });
    Ok(slots)
}

/// `revision`'s stack onto `destination`, judged as the gate would, with
/// no check of its changes' proposals (R3.45, #328: `EvaluatePolicy` over a
/// stack). Reads only.
pub(crate) async fn judge_stack(
    state: &AppState,
    request: &LandRequest,
    destination: &str,
    policy: Option<Vec<u8>>,
) -> Result<Vec<Decided>, LandingError> {
    let repo = request.repo;
    let info = {
        let request = request.clone();
        state
            .manager
            .handle(repo)?
            .call_async(move |head| async move { resolve(head, &request).await }.boxed_local())
            .await??
    };
    let members = read_stack(state, repo, &info, destination).await?;
    let mut slots: Vec<Slot> = members
        .into_iter()
        .map(|member| Slot {
            target: LandTarget {
                change: member.change,
                commit: member.commit,
                bookmark: destination.to_owned(),
            },
            tree: member.tree,
            parents: member.parents,
            open: None,
        })
        .collect();
    if slots.is_empty() {
        slots.push(Slot {
            target: LandTarget {
                change: info.change_id.clone(),
                commit: info.commit_id.clone(),
                bookmark: destination.to_owned(),
            },
            tree: info.tree_id.clone(),
            parents: Vec::new(),
            open: None,
        });
    }
    let who = requester(state, request);
    let context = JudgeContext {
        policy,
        ..JudgeContext::default()
    };
    let (decided, _) = judge_slots(state, repo, &slots, destination, &who, &context).await?;
    Ok(decided)
}

/// `decided` read again under the lifecycle lock (R3.10): a change another
/// request queued meanwhile is answered with that landing.
fn recheck(
    state: &AppState,
    repo: RepoId,
    decided: Vec<Decided>,
) -> Result<Vec<Decided>, LandingError> {
    let mut checked = Vec::with_capacity(decided.len());
    for decision in decided {
        checked.push(match decision {
            Decided::Pass {
                target,
                row,
                judged,
            } => {
                let change = target.change.as_str();
                let now = state.db.lifecycle_row(repo, change)?;
                let proposed = now.as_ref().is_some_and(|now| {
                    now.state == LifecycleState::Proposed.as_str()
                        && now.commit_id == target.commit.as_str()
                });
                if proposed {
                    Decided::Pass {
                        target,
                        row,
                        judged,
                    }
                } else if let Some(open) = state.db.open_landing_of(repo, change)? {
                    Decided::Open(Box::new(open))
                } else {
                    return Err(LandingError::NotProposed {
                        change: change.to_owned(),
                        state: now
                            .and_then(|row| row.state.parse().ok())
                            .unwrap_or(LifecycleState::Draft),
                        head: None,
                    });
                }
            }
            other => other,
        });
    }
    Ok(checked)
}

/// Queues the changes `decided` passes, in one operation (R3.10), and
/// writes every new row of the request in one transaction, under the
/// lifecycle lock, so the worker never sees part of a stack. A change
/// another request queued meanwhile is answered with that landing.
async fn enqueue(
    state: &AppState,
    request: &LandRequest,
    who: &Requester,
    destination: &str,
    decided: Vec<Decided>,
) -> Result<(Option<OperationIdHex>, Vec<StoredLanding>), LandingError> {
    let repo = request.repo;
    let create_lock = state.manager.repo_lock(repo);
    let _shared = create_lock.read().await;
    let lifecycle_lock = state.lifecycle.lock(repo);
    let _held = lifecycle_lock.lock().await;
    // Read again under the lock: another request may have queued a change.
    let checked = recheck(state, repo, decided)?;
    let passes: Vec<&LandTarget> = checked
        .iter()
        .filter_map(|decision| match decision {
            Decided::Pass { target, .. } => Some(target),
            _ => None,
        })
        .collect();
    let mut op = None;
    if let Some(last) = passes.last() {
        if state.db.queued_landing_count(repo)? + passes.len() as u64 > MAX_QUEUED {
            return Err(LandingError::QueueFull);
        }
        let mut rows = Vec::with_capacity(passes.len());
        for target in &passes {
            let change = target.change.as_str();
            let round = state
                .db
                .lifecycle_row(repo, change)?
                .map_or(0, |row| row.round);
            rows.push(transition_row(
                repo,
                change,
                target.commit.as_str(),
                LifecycleState::Proposed,
                LifecycleState::Queued,
                round,
                destination,
                None,
                who,
                Actor::parse(who.actor.as_deref().unwrap_or("agent")),
            ));
        }
        let mut description = oplog::transition_description("queue", last.change.as_str());
        if rows.len() > 1 {
            description = format!("{description} (stack of {})", rows.len());
        }
        let mut meta = OpMeta::new(OpKind::Transition, description);
        meta.workspace = request.workspace.as_ref().map(|record| record.id.as_hex());
        meta.agent.clone_from(&who.marked);
        let recorded = crate::lifecycle::record_daemon_transitions(state, repo, rows, meta).await?;
        op = Some(recorded.op_id);
    }
    let mut fresh: Vec<(LandingRow, Vec<LandingChangeRow>)> = Vec::new();
    let mut ids: Vec<String> = Vec::with_capacity(checked.len());
    for decision in checked {
        match decision {
            Decided::Open(open) => ids.push(open.row.id.clone()),
            Decided::Pass { row, .. } | Decided::Block { row, .. } => {
                let (mut row, changes) = *row;
                if row.outcome == Outcome::Queued.as_str() {
                    row.enqueue_operation_id = op.as_ref().map(ToString::to_string);
                }
                ids.push(row.id.clone());
                fresh.push((row, changes));
            }
        }
    }
    state.db.write_landings(&fresh)?;
    if let Some(op) = &op {
        tracing::info!(repo = %repo, op_id = %op, queued = fresh.iter().filter(|(row, _)| row.outcome == Outcome::Queued.as_str()).count(), "queued for landing");
    }
    let mut written = Vec::with_capacity(ids.len());
    for id in ids {
        written.push(
            state
                .db
                .landing(repo, &id)?
                .ok_or_else(|| LandingError::NotFound(id.clone()))?,
        );
    }
    Ok((op, written))
}

/// Records the blocked landing `row` of a change landed alone, or counts
/// one more attempt on the change's latest identical one (S-125).
fn blocked(
    state: &AppState,
    repo: RepoId,
    target: &LandTarget,
    row: &LandingRow,
    change: &LandingChangeRow,
) -> Result<StoredLanding, LandingError> {
    let now = Timestamp::now();
    if let Some(latest) = state.db.latest_landing_of(repo, target.change.as_str())?
        && latest.row.outcome == Outcome::Blocked.as_str()
        && latest.row.reason == row.reason
        && latest.row.message == row.message
        && latest.row.policy_sha256 == row.policy_sha256
        && latest.row.stack.is_none()
        && latest.changes.first().is_some_and(|first| {
            first.queued_commit == change.queued_commit
                && first.evaluation_sha256 == change.evaluation_sha256
                && first.conflicts == change.conflicts
        })
    {
        state.db.bump_landing_attempts(repo, &latest.row.id, now)?;
        return state
            .db
            .landing(repo, &latest.row.id)?
            .ok_or_else(|| LandingError::NotFound(latest.row.id.clone()));
    }
    state.db.write_landing(row, std::slice::from_ref(change))?;
    tracing::info!(change_id = %target.change, landing_id = %row.id, reason = row.reason.as_deref().unwrap_or_default(), "landing blocked at the gate");
    state
        .db
        .landing(repo, &row.id)?
        .ok_or_else(|| LandingError::NotFound(row.id.clone()))
}

/// Instrumenting a future with a span, without importing the trait at
/// every call.
trait InstrumentWith: Sized {
    fn instrument_with(self, span: tracing::Span) -> tracing::instrument::Instrumented<Self>;
}

impl<F: Future> InstrumentWith for F {
    fn instrument_with(self, span: tracing::Span) -> tracing::instrument::Instrumented<Self> {
        tracing::Instrument::instrument(self, span)
    }
}

/// The change id `text` names, for reads: the change's id as given.
pub(crate) fn change_hex(text: &str) -> Option<ChangeIdHex> {
    text.parse().ok()
}

/// A commit id as given.
pub(crate) fn commit_hex(text: &str) -> Option<CommitIdHex> {
    text.parse().ok()
}

impl Landings {
    /// Marks `repo`'s worker running; `false` when it already was (it is
    /// then woken to look again).
    fn claim(&self, repo: RepoId) -> bool {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(dirty) = running.get_mut(&repo) {
            *dirty = true;
            return false;
        }
        running.insert(repo, false);
        true
    }

    /// Whether `repo`'s worker should stop: no wake-up since it last
    /// looked. When it stops, it is no longer running.
    fn release(&self, repo: RepoId) -> bool {
        let mut running = self.running.lock().unwrap_or_else(PoisonError::into_inner);
        match running.get_mut(&repo) {
            Some(dirty) if *dirty => {
                *dirty = false;
                false
            }
            _ => {
                running.remove(&repo);
                true
            }
        }
    }
}

/// One change of a stack as `EvaluatePolicy` reports it (R3.45, #328).
pub(crate) fn member_evaluation(decision: &Decided, position: usize) -> v1::StackMemberEvaluation {
    let position = u32::try_from(position).unwrap_or(u32::MAX);
    match decision {
        Decided::Open(open) => {
            let first = open.changes.first();
            v1::StackMemberEvaluation {
                change_id: first
                    .map(|change| change.change_id.clone())
                    .unwrap_or_default(),
                commit_id: first
                    .map(|change| change.queued_commit.clone())
                    .unwrap_or_default(),
                position,
                passes: true,
                reason: String::new(),
                message: format!("queued already (landing {})", open.row.id),
                evaluation: None,
            }
        }
        Decided::Pass { target, judged, .. } => v1::StackMemberEvaluation {
            change_id: target.change.as_str().to_owned(),
            commit_id: target.commit.as_str().to_owned(),
            position,
            passes: true,
            reason: String::new(),
            message: "allowed".to_owned(),
            evaluation: Some(crate::service::policy::evaluation_message(
                &judged.evaluation,
            )),
        },
        Decided::Block {
            target,
            row,
            judged,
        } => v1::StackMemberEvaluation {
            change_id: target.change.as_str().to_owned(),
            commit_id: target.commit.as_str().to_owned(),
            position,
            passes: false,
            reason: row.0.reason.clone().unwrap_or_default(),
            message: row.0.message.clone().unwrap_or_default(),
            evaluation: judged
                .as_ref()
                .map(|judged| crate::service::policy::evaluation_message(&judged.evaluation)),
        },
    }
}
