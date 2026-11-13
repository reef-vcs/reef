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

mod batch;
mod history;
pub mod messages;
mod worker;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
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

pub use batch::{AGENT as BATCH_AGENT, BATCH_PRIORITY, BATCH_QUEUE_TIMEOUT, MAX_TRIAL_ATTEMPTS};
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
}

impl Classify for LandingError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotProposed { .. }
            | Self::Amended { .. }
            | Self::DestinationNotLocal(_)
            | Self::DestinationNotFound(_)
            | Self::DestinationConflicted(_) => ErrorKind::Conflict,
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
            Self::NotProposed { change, state }
                if *state == LifecycleState::Draft
                    || *state == LifecycleState::Withdrawn
                    || *state == LifecycleState::Declined =>
            {
                Some(Advice::run(
                    "Propose it first; proposing is the request to land.",
                    ["reef", "propose", change.as_str()],
                ))
            }
            Self::NotProposed { change, state } if *state == LifecycleState::Queued => {
                Some(Advice::run(
                    "It is already queued; follow its landing.",
                    ["reef", "landing", "show", change.as_str()],
                ))
            }
            Self::NotProposed { .. } => Some(Advice::hint(
                "A landed or superseded change is final; make a new change instead.",
            )),
            Self::Amended { change, .. } => Some(Advice::run(
                "The change moved since it was proposed; propose it again to record the new commit, then land it.",
                ["reef", "propose", change.as_str()],
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
            _ => None,
        }
    }
}

impl LandingError {
    /// Its reason and metadata, as `crate::error` reports them.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            Self::NotProposed { change, state } => (
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
}

/// What [`land_change`] did.
#[derive(Debug)]
pub struct LandOutcome {
    /// The landing as it was when the wait ended.
    pub landing: StoredLanding,
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
#[derive(Clone, Debug, Default)]
pub(crate) struct JudgeContext {
    pub(crate) landing: Option<reef_policy::input::Landing>,
    pub(crate) batch: reef_policy::Batch,
    pub(crate) evidence_tree: Option<String>,
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
/// there over the queued commit (R3.13). Reads only.
pub(crate) async fn judge(
    state: &AppState,
    repo: RepoId,
    target: &LandTarget,
) -> Result<Verdict, LandingError> {
    judge_with(state, repo, target, &JudgeContext::default()).await
}

/// [`judge`], seeing what `context` says besides the change (R3.14).
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
        handle
            .call_async(move |head| {
                async move {
                    let repo = head.refresh().await?;
                    let preview = land::preview(&repo, &target).await;
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
    let paths = change.change.paths.clone();
    let evidence_tree = context.evidence_tree.as_ref().or(change.tree_id.as_ref());
    let evidence = match evidence_tree {
        Some(tree) => policy_input::read_evidence(state, repo, tree)
            .await
            .map_err(|status| LandingError::Input(status.message().to_owned()))?,
        None => policy_input::EvidenceRead {
            evidence: Vec::new(),
            ignored: 0,
            invalid: Vec::new(),
        },
    };
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
    let policy_file = read.policy;
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
        error @ LandError::UnlandedParents { .. } => {
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

/// Asks for a landing; see the module docs. Runs as its own task, so a
/// caller that stops waiting cannot cut it between its enqueue's operation
/// and its row.
pub async fn land_change(
    state: &Arc<AppState>,
    request: LandRequest,
) -> Result<LandOutcome, LandingError> {
    if request.wait > MAX_WAIT {
        return Err(LandingError::InvalidWait);
    }
    let started = Instant::now();
    let wait = request.wait;
    let task_state = Arc::clone(state);
    let repo = request.repo;
    let span = tracing::Span::current();
    let mut outcome = tokio::spawn(
        async move {
            let outcome = gate(&task_state, request).await?;
            // Woken here, so a caller that goes away cannot leave a queued
            // change with no worker.
            if outcome.landing.row.outcome == Outcome::Queued.as_str() {
                worker::wake(&task_state, outcome.landing.row.repo_id);
            }
            Ok::<_, LandingError>(outcome)
        }
        .instrument_with(span),
    )
    .await
    .map_err(|_| LandingError::Thread(RepoThreadError::Panicked { id: repo }))??;
    if outcome.landing.row.outcome == Outcome::Queued.as_str() {
        let deadline = started + wait;
        outcome.landing = wait_for(state, repo, &outcome.landing.row.id, deadline).await?;
    }
    Ok(outcome)
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

/// The gate: the checks, the judgment, and the enqueue.
#[expect(clippy::too_many_lines, reason = "the gate's steps in order")]
async fn gate(state: &AppState, request: LandRequest) -> Result<LandOutcome, LandingError> {
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
        return Ok(LandOutcome {
            landing: open,
            enqueue: None,
            change: Some(info),
        });
    }
    let Some(row) = row.filter(|_| current == LifecycleState::Proposed) else {
        return Err(LandingError::NotProposed {
            change,
            state: current,
        });
    };
    if row.commit_id != commit.as_str() {
        return Err(LandingError::Amended {
            change,
            commit: row.commit_id,
            visible: commit.as_str().to_owned(),
        });
    }
    let destination = row.destination.clone().unwrap_or_default();
    if destination.contains('@') {
        return Err(LandingError::DestinationNotLocal(destination));
    }
    let target = LandTarget {
        change: info.change_id.clone(),
        commit: commit.clone(),
        bookmark: destination.clone(),
    };
    let who = requester(state, &request);
    let verdict = judge(state, repo, &target).await?;
    let judged = match verdict {
        Verdict::Block {
            problem: Some(DestinationProblem::Missing),
            ..
        } => return Err(LandingError::DestinationNotFound(destination)),
        Verdict::Block {
            problem: Some(DestinationProblem::Conflicted),
            ..
        } => return Err(LandingError::DestinationConflicted(destination)),
        Verdict::Block {
            reason: Reason::Amended,
            ..
        } => {
            // The change moved between the gate's read and its preview.
            return Err(LandingError::Amended {
                change,
                commit: commit.as_str().to_owned(),
                visible: "(moved during the request)".to_owned(),
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
            let landing = blocked(
                state,
                repo,
                &target,
                &who,
                (reason, message),
                (preview.as_ref(), judged.as_deref(), head),
                info.tree_id.clone(),
            )?;
            return Ok(LandOutcome {
                landing,
                enqueue: None,
                change: Some(info),
            });
        }
        Verdict::Allow(judged) => judged,
    };

    // Queue it: the row must still be proposed at the commit judged.
    let create_lock = state.manager.repo_lock(repo);
    let _shared = create_lock.read().await;
    let lifecycle_lock = state.lifecycle.lock(repo);
    let _held = lifecycle_lock.lock().await;
    let row = state.db.lifecycle_row(repo, &change)?.filter(|row| {
        row.state == LifecycleState::Proposed.as_str() && row.commit_id == commit.as_str()
    });
    let Some(row) = row else {
        // Another request queued it meanwhile: answer with its landing.
        if let Some(open) = state.db.open_landing_of(repo, &change)? {
            return Ok(LandOutcome {
                landing: open,
                enqueue: None,
                change: Some(info),
            });
        }
        let now = state.db.lifecycle_row(repo, &change)?;
        return Err(LandingError::NotProposed {
            change,
            state: now
                .and_then(|row| row.state.parse().ok())
                .unwrap_or(LifecycleState::Draft),
        });
    };
    if state.db.queued_landing_count(repo)? >= MAX_QUEUED {
        return Err(LandingError::QueueFull);
    }
    let transition = transition_row(
        repo,
        &change,
        commit.as_str(),
        LifecycleState::Proposed,
        LifecycleState::Queued,
        row.round,
        &destination,
        None,
        &who,
        Actor::parse(who.actor.as_deref().unwrap_or("agent")),
    );
    let mut meta = OpMeta::new(
        OpKind::Transition,
        oplog::transition_description("queue", &change),
    );
    meta.workspace = request.workspace.as_ref().map(|record| record.id.as_hex());
    meta.agent.clone_from(&who.marked);
    let recorded =
        crate::lifecycle::record_daemon_transition(state, repo, transition, meta).await?;
    let mut landing_row = new_row(repo, &destination, &who);
    landing_row.destination_before = Some(judged.preview.destination.as_str().to_owned());
    landing_row.policy_commit = Some(judged.policy.commit.clone());
    landing_row.policy_blob.clone_from(&judged.policy.blob);
    landing_row.policy_sha256.clone_from(&judged.policy.sha256);
    landing_row.enqueue_operation_id = Some(recorded.op_id.to_string());
    let changes = vec![change_row(
        &target,
        info.tree_id.clone(),
        None,
        Some(&judged),
    )];
    state.db.write_landing(&landing_row, &changes)?;
    tracing::info!(change_id = %change, landing_id = %landing_row.id, op_id = %recorded.op_id, "queued for landing");
    let landing = state
        .db
        .landing(repo, &landing_row.id)?
        .ok_or_else(|| LandingError::NotFound(landing_row.id.clone()))?;
    Ok(LandOutcome {
        landing,
        enqueue: Some(recorded.op_id),
        change: Some(info),
    })
}

/// Records a blocked landing, or counts one more attempt on the change's
/// latest identical one (S-125).
fn blocked(
    state: &AppState,
    repo: RepoId,
    target: &LandTarget,
    who: &Requester,
    (reason, message): (Reason, String),
    (preview, judged, head): (Option<&Preview>, Option<&Judged>, Option<String>),
    queued_tree: Option<String>,
) -> Result<StoredLanding, LandingError> {
    let change = change_row(target, queued_tree, preview, judged);
    let now = Timestamp::now();
    if let Some(latest) = state.db.latest_landing_of(repo, target.change.as_str())?
        && latest.row.outcome == Outcome::Blocked.as_str()
        && latest.row.reason.as_deref() == Some(reason.as_str())
        && latest.row.policy_sha256.as_deref()
            == judged.and_then(|judged| judged.policy.sha256.as_deref())
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
    let mut row = new_row(repo, &target.bookmark, who);
    Outcome::Blocked.as_str().clone_into(&mut row.outcome);
    row.reason = Some(reason.as_str().to_owned());
    row.message = Some(message);
    row.destination_before = head;
    if let Some(judged) = judged {
        row.policy_commit = Some(judged.policy.commit.clone());
        row.policy_blob.clone_from(&judged.policy.blob);
        row.policy_sha256.clone_from(&judged.policy.sha256);
    }
    row.finished_at = Some(now);
    state
        .db
        .write_landing(&row, std::slice::from_ref(&change))?;
    tracing::info!(change_id = %target.change, landing_id = %row.id, reason = reason.as_str(), "landing blocked at the gate");
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
