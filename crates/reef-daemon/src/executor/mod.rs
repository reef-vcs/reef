//! The executor protocol's job queue (R3.05, ADR 0059;
//! `docs/executor-protocol.md`).
//!
//! A job is one request for checks on one tree. Requesters submit and
//! cancel jobs; executors, processes outside Reef that dial the daemon,
//! claim a job under a lease, report progress and signed evidence, and
//! complete it. The daemon never connects to an executor and never sends it
//! a command.
//!
//! Jobs are rows of `reef.sqlite3` ([`crate::db::JobRow`]), so they survive
//! a restart and a restore. Every transition is one immediate transaction
//! that reads the job again inside it (`Executor::transition`): that is
//! the serialization, with no per-job lock. The timer ([`Executor::tick`])
//! acts only through those transactions, and a transition that finds the
//! row already moved does nothing. It reads the next due time from the open
//! rows, at most 1,000 (`[executor] max_open_jobs`), so memory and the
//! database cannot disagree.
//!
//! Progress reaches subscribers as daemon-reported presence entries of kind
//! `JOB`, one per open job ([`presence`]).
//!
//! Times come from [`Executor::now`], the wall clock plus an offset tests
//! move forward (`Executor::advance`, with the `testing` feature) so that
//! no test sleeps for a lease.

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngExt as _;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::config::ExecutorSection;
use crate::db::{
    Db, DbError, ExecutorColumns, JobEvidenceRow, JobRow, JobState, job_in, queued_jobs_in,
    update_job_in,
};
use crate::error::{Advice, Advise};
use crate::model::Timestamp;
use crate::presence::PresenceHub;
use crate::service::evidence::EvidenceError;

pub mod accept;
mod audit;
pub mod presence;
pub mod retention;
pub mod submit;

pub use accept::{Accepted, Completion, Update};
pub use submit::{NewJob, SpecLimits};

/// The fixed grace after a running attempt's wall time before the daemon
/// expires it: time for the executor's last `CompleteJob`.
pub const DEADLINE_GRACE: Duration = Duration::from_secs(60);
/// The lease a claim gets when it names none.
pub const DEFAULT_LEASE: Duration = Duration::from_secs(60);
/// The shortest lease a claim may ask for.
pub const MIN_LEASE: Duration = Duration::from_secs(15);
/// The longest lease a claim may ask for.
pub const MAX_LEASE: Duration = Duration::from_mins(10);
/// The longest a claim waits for a job.
pub const MAX_WAIT: Duration = Duration::from_secs(60);
/// How long before a claim an object's `startedOn` may be for it to count
/// as fresh.
pub const FRESH_SLACK: Duration = Duration::from_mins(5);
/// Evidence objects one `UpdateJob` or `CompleteJob` carries at most.
pub const MAX_EVIDENCE_PER_CALL: usize = 50;
/// Missing pairs a `JOB_INCOMPLETE` names.
const MISSING_SHOWN: usize = 10;
/// Claims one remote token may have waiting at once: half of its request
/// slots (`remote::MAX_REQUESTS_PER_TOKEN`). Past it, a claim answers at
/// once.
pub const MAX_WAITING_PER_TOKEN: usize = 16;
/// The longest the timer sleeps without looking again.
const TIMER_CEILING: Duration = Duration::from_secs(30);

/// Why an executor request is refused, with its reason (ADR 0059).
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ExecutorError {
    /// A job request out of its bounds: `INVALID_JOB_SPEC`.
    #[error("{field}: {message}")]
    InvalidSpec {
        /// The field, as the API names it.
        field: &'static str,
        /// What is wrong with it.
        message: String,
    },
    /// Past `[executor] max_open_jobs`: `TOO_MANY_JOBS`.
    #[error("{limit} jobs are already queued or running, the limit ([executor] max_open_jobs)")]
    TooMany {
        /// The limit.
        limit: u32,
    },
    /// A claim's executor fields out of their bounds: `INVALID_EXECUTOR`.
    #[error("executor {field}: {message}")]
    InvalidExecutor {
        /// The field.
        field: &'static str,
        /// What is wrong with it.
        message: String,
    },
    /// A remote claim through a token linked to no identity:
    /// `EXECUTOR_REFUSED`.
    #[error("this remote token is linked to no operator identity, so it cannot claim jobs")]
    TokenNotLinked,
    /// Not a job id: `INVALID_JOB_ID`.
    #[error("job id {job_id:?} is not `job_` and {} hex digits", if *prefix { "4 to 16" } else { "16" })]
    InvalidId {
        /// The text.
        job_id: String,
        /// A prefix was accepted.
        prefix: bool,
    },
    /// A prefix of several jobs: `AMBIGUOUS_JOB_ID`.
    #[error("job id {job_id:?} is ambiguous; it matches {}", candidates.join(", "))]
    Ambiguous {
        /// The prefix.
        job_id: String,
        /// Up to ten of the ids it matches.
        candidates: Vec<String>,
    },
    /// No job has the id: `JOB_NOT_FOUND`.
    #[error("no job is {job_id}")]
    NotFound {
        /// The id or prefix.
        job_id: String,
    },
    /// A remote call on another token's job: `JOB_NOT_OWNED`.
    #[error("job {job_id} was {} through another remote token", if *claimed { "claimed" } else { "submitted" })]
    NotOwned {
        /// The job.
        job_id: String,
        /// The rule was the claim's (`OwnClaim`), not the submission's.
        claimed: bool,
    },
    /// A claim token that does not hold the job: `JOB_NOT_CLAIMED`.
    #[error("job {job_id} is not held by this claim ({state})")]
    NotClaimed {
        /// The job.
        job_id: String,
        /// `wrong_token`, `lapsed`, `cancelled`, `completed` or `expired`.
        state: &'static str,
    },
    /// A cancellation of a finished job: `JOB_FINISHED`.
    #[error("job {job_id} has already {state}", state = if *state == "completed" { "completed" } else { "expired" })]
    Finished {
        /// The job.
        job_id: String,
        /// `completed` or `expired`.
        state: &'static str,
    },
    /// Evidence about another tree, or of a kind not requested:
    /// `JOB_EVIDENCE_MISMATCH`.
    #[error("evidence {index} ({evidence_id}) is {}", if *field == "tree" {
        format!("about tree {actual}, not the job's tree {expected}")
    } else {
        format!("of kind {actual:?}, which the job did not request ({expected})")
    })]
    Mismatch {
        /// Its position in the call.
        index: usize,
        /// `tree` or `kind`.
        field: &'static str,
        /// Its id.
        evidence_id: String,
        /// What the evidence says.
        actual: String,
        /// What the job requested.
        expected: String,
    },
    /// `DONE` with pairs missing: `JOB_INCOMPLETE`.
    #[error("{count} requested (kind, target) pairs have no evidence yet: {}", missing.join(", "), count = missing_count)]
    Incomplete {
        /// Up to ten `kind:target` pairs.
        missing: Vec<String>,
        /// How many are missing.
        missing_count: usize,
    },
    /// An update, completion or cancellation out of its bounds:
    /// `INVALID_JOB_UPDATE`.
    #[error("{field}: {message}")]
    InvalidUpdate {
        /// The field.
        field: &'static str,
        /// What is wrong with it.
        message: String,
    },
    /// An envelope a call carries is not evidence: R3.01's reasons, with
    /// `index`.
    #[error("evidence {index}: {source}")]
    Evidence {
        /// Its position in the call.
        index: usize,
        /// Why.
        #[source]
        source: EvidenceError,
    },
    /// The tree or revision a submission names: R3.01's reasons.
    #[error(transparent)]
    Tree(EvidenceError),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
    /// A task on the blocking pool did not finish.
    #[error("an executor task failed: {0}")]
    Task(String),
}

impl Classify for ExecutorError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidSpec { .. }
            | Self::InvalidExecutor { .. }
            | Self::InvalidId { .. }
            | Self::Ambiguous { .. }
            | Self::InvalidUpdate { .. } => ErrorKind::InvalidArgument,
            Self::TooMany { .. } => ErrorKind::Unavailable,
            Self::TokenNotLinked | Self::NotOwned { .. } => ErrorKind::PermissionDenied,
            Self::NotFound { .. } => ErrorKind::NotFound,
            Self::NotClaimed { .. }
            | Self::Finished { .. }
            | Self::Mismatch { .. }
            | Self::Incomplete { .. } => ErrorKind::Conflict,
            Self::Evidence { source, .. } | Self::Tree(source) => source.kind(),
            Self::Db(error) => error.kind(),
            Self::Task(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for ExecutorError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::TooMany { .. } => Some(Advice::run(
                "Cancel jobs nobody needs, or raise [executor] max_open_jobs in the daemon's config.toml.",
                ["reef", "job", "list", "--queue"],
            )),
            Self::TokenNotLinked => Some(Advice::hint(
                "On the daemon's machine, create a token linked to an identity: `reef remote token create <name> --scope write --identity <ci>`.",
            )),
            Self::NotFound { .. } => Some(Advice::run(
                "List the jobs to find its id; finished jobs are deleted after [executor] job_retention.",
                ["reef", "job", "list"],
            )),
            Self::NotOwned { .. } => Some(Advice::hint(
                "Through the remote listener, a job is updated and completed only through the token that claimed it, and cancelled only through the one that submitted it.",
            )),
            Self::NotClaimed { .. } => Some(Advice::hint(
                "Stop work on this job and do not retry: its claim ended (it lapsed, was cancelled or released, or the job finished).",
            )),
            Self::Mismatch { field, .. } => Some(Advice::hint(if *field == "tree" {
                "A job's evidence must be about the job's tree_id; materialize exactly that tree and sign claims about it."
            } else {
                "Send evidence only of the kinds the job requested."
            })),
            Self::Incomplete { .. } => Some(Advice::hint(
                "Send one summary object for each missing (kind, target) pair, with result `error` for a check that reached no verdict, or complete with BUDGET_EXHAUSTED or FAILED.",
            )),
            Self::Evidence { source, .. } | Self::Tree(source) => source.advice(),
            Self::Db(error) => error.advice(),
            _ => None,
        }
    }
}

impl ExecutorError {
    /// The reason and metadata of `ErrorDetail`, for `crate::error`.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        use reef_api::error as reason;
        Some(match self {
            Self::InvalidSpec { field, .. } => (
                reason::INVALID_JOB_SPEC,
                vec![("field", (*field).to_owned())],
            ),
            Self::TooMany { limit } => (reason::TOO_MANY_JOBS, vec![("limit", limit.to_string())]),
            Self::InvalidExecutor { field, .. } => (
                reason::INVALID_EXECUTOR,
                vec![("field", (*field).to_owned())],
            ),
            Self::TokenNotLinked => (
                reason::EXECUTOR_REFUSED,
                vec![("rule", "token_not_linked".to_owned())],
            ),
            Self::InvalidId { job_id, .. } => {
                (reason::INVALID_JOB_ID, vec![("job_id", job_id.clone())])
            }
            Self::Ambiguous { job_id, candidates } => (
                reason::AMBIGUOUS_JOB_ID,
                vec![
                    ("job_id", job_id.clone()),
                    ("job_ids", candidates.join(",")),
                ],
            ),
            Self::NotFound { job_id } => (reason::JOB_NOT_FOUND, vec![("job_id", job_id.clone())]),
            Self::NotOwned { job_id, .. } => {
                (reason::JOB_NOT_OWNED, vec![("job_id", job_id.clone())])
            }
            Self::NotClaimed { job_id, state } => (
                reason::JOB_NOT_CLAIMED,
                vec![("job_id", job_id.clone()), ("state", (*state).to_owned())],
            ),
            Self::Finished { job_id, state } => (
                reason::JOB_FINISHED,
                vec![("job_id", job_id.clone()), ("state", (*state).to_owned())],
            ),
            Self::Mismatch {
                index,
                field,
                evidence_id,
                ..
            } => (
                reason::JOB_EVIDENCE_MISMATCH,
                vec![
                    ("index", index.to_string()),
                    ("field", (*field).to_owned()),
                    ("evidence_id", evidence_id.clone()),
                ],
            ),
            Self::Incomplete {
                missing,
                missing_count,
            } => (
                reason::JOB_INCOMPLETE,
                vec![
                    ("missing", missing.join(",")),
                    ("missing_count", missing_count.to_string()),
                ],
            ),
            Self::InvalidUpdate { field, .. } => (
                reason::INVALID_JOB_UPDATE,
                vec![("field", (*field).to_owned())],
            ),
            Self::Evidence { index, source } => {
                let (reason, mut metadata) = source.reason()?;
                metadata.push(("index", index.to_string()));
                (reason, metadata)
            }
            Self::Tree(source) => return source.reason(),
            Self::Db(_) | Self::Task(_) => return None,
        })
    }
}

impl From<DbError> for Box<ExecutorError> {
    fn from(error: DbError) -> Self {
        Box::new(ExecutorError::Db(error))
    }
}

/// Lowercase `job_<16 hex>` from `text`, or `INVALID_JOB_ID`.
pub fn full_id(text: &str) -> Result<String, ExecutorError> {
    let text = text.trim();
    let invalid = || ExecutorError::InvalidId {
        job_id: text.to_owned(),
        prefix: false,
    };
    let hex = text.strip_prefix("job_").ok_or_else(invalid)?;
    if hex.len() != 16 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    Ok(format!("job_{}", hex.to_ascii_lowercase()))
}

/// The hex after `job_` in `text`, 4 to 16 digits, lowercase; or
/// `INVALID_JOB_ID`.
pub fn id_prefix(text: &str) -> Result<String, ExecutorError> {
    let text = text.trim();
    let invalid = || ExecutorError::InvalidId {
        job_id: text.to_owned(),
        prefix: true,
    };
    let hex = text.strip_prefix("job_").ok_or_else(invalid)?;
    if !(4..=16).contains(&hex.len()) || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(invalid());
    }
    Ok(hex.to_ascii_lowercase())
}

/// The presence key of a job: the 64 bits of its id.
fn presence_key(id: &str) -> u64 {
    id.strip_prefix("job_")
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())
        .unwrap_or(0)
}

/// A new claim token for `attempt`: the attempt, a dot, and 32 random
/// bytes in base64url. The attempt in the token is what tells `lapsed`
/// from `wrong_token`; the secret is the random part.
fn new_claim_token(attempt: u32) -> String {
    let secret: [u8; 32] = rand::rng().random();
    format!("{attempt}.{}", URL_SAFE_NO_PAD.encode(secret))
}

/// The digest a claim token is kept as.
fn claim_digest(token: &str) -> blake3::Hash {
    blake3::hash(token.as_bytes())
}

/// The attempt a claim token names, if it is one.
fn token_attempt(token: &str) -> Option<u32> {
    token.split_once('.')?.0.parse().ok()
}

/// The state a claim check fails with, as `JOB_NOT_CLAIMED`'s metadata
/// names it; `None` when `token` holds `job`.
fn claim_problem(job: &JobRow, token: &str) -> Option<&'static str> {
    match job.state {
        JobState::Cancelled => return Some("cancelled"),
        JobState::Expired => return Some("expired"),
        JobState::Completed => return Some("completed"),
        JobState::Queued | JobState::Running => {}
    }
    let Some(attempt) = token_attempt(token) else {
        return Some("wrong_token");
    };
    if job.state == JobState::Queued || attempt < job.attempt {
        return Some(if attempt <= job.attempt && job.attempt > 0 {
            "lapsed"
        } else {
            "wrong_token"
        });
    }
    if !digest_matches(job, token) {
        return Some("wrong_token");
    }
    None
}

/// Whether `token` is the job's current claim token, in constant time.
fn digest_matches(job: &JobRow, token: &str) -> bool {
    job.claim_digest
        .as_deref()
        .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
        .is_some_and(|stored| blake3::Hash::from_bytes(stored) == claim_digest(token))
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

/// What a claim declares, validated.
#[derive(Clone, Debug)]
pub struct Claimant {
    /// The executor's declared fields.
    pub columns: ExecutorColumns,
    /// Only jobs of these repositories; empty for all.
    pub repos: Vec<crate::model::RepoId>,
    /// The claim's lease.
    pub lease: Duration,
}

impl Claimant {
    /// Whether this claimant may claim `job`.
    fn matches(&self, job: &JobRow) -> bool {
        job.kinds
            .iter()
            .all(|kind| self.columns.kinds.contains(kind))
            && job
                .requirements
                .iter()
                .all(|label| self.columns.labels.contains(label))
            && (self.repos.is_empty() || self.repos.contains(&job.repo_id))
            && job.protocol <= self.columns.protocol
    }
}

/// A waiting claim's place under its token's cap; `None` when the token
/// is at [`MAX_WAITING_PER_TOKEN`]. The socket has no cap.
struct Waiting<'a> {
    map: &'a Mutex<HashMap<String, usize>>,
    token: Option<String>,
}

impl<'a> Waiting<'a> {
    fn enter(map: &'a Mutex<HashMap<String, usize>>, token: Option<&str>) -> Option<Waiting<'a>> {
        let Some(token) = token else {
            return Some(Waiting { map, token: None });
        };
        let mut waiting = map.lock().unwrap_or_else(PoisonError::into_inner);
        let count = waiting.entry(token.to_owned()).or_default();
        if *count >= MAX_WAITING_PER_TOKEN {
            return None;
        }
        *count += 1;
        Some(Waiting {
            map,
            token: Some(token.to_owned()),
        })
    }
}

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        let Some(token) = &self.token else { return };
        let mut waiting = self.map.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(count) = waiting.get_mut(token) {
            *count -= 1;
            if *count == 0 {
                waiting.remove(token);
            }
        }
    }
}

/// The executor protocol's state: the database, the presence hub, and the
/// signals claims and the timer wait on.
pub struct Executor {
    db: Arc<Db>,
    presence: Arc<PresenceHub>,
    settings: ExecutorSection,
    /// Added to the wall clock: tests move it forward.
    offset_ms: AtomicI64,
    /// Signalled by every submission and requeue; waiting claims wake.
    queued: Notify,
    /// Signalled when a due time may have moved earlier.
    timer: Notify,
    /// Waiting claims by remote token.
    waiting: Mutex<HashMap<String, usize>>,
}

impl std::fmt::Debug for Executor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Executor")
            .field("settings", &self.settings)
            .finish_non_exhaustive()
    }
}

impl Executor {
    /// The queue over `db`, publishing to `presence`.
    pub fn new(db: Arc<Db>, presence: Arc<PresenceHub>, settings: ExecutorSection) -> Executor {
        Executor {
            db,
            presence,
            settings,
            offset_ms: AtomicI64::new(0),
            queued: Notify::new(),
            timer: Notify::new(),
            waiting: Mutex::new(HashMap::new()),
        }
    }

    /// The configuration.
    pub fn settings(&self) -> &ExecutorSection {
        &self.settings
    }

    /// Now, as the queue sees it.
    pub fn now(&self) -> Timestamp {
        Timestamp(
            Timestamp::now()
                .0
                .saturating_add(self.offset_ms.load(Ordering::Relaxed)),
        )
    }

    /// Moves the queue's clock forward by `by`, for tests: leases, deadlines
    /// and queue timeouts pass without a test sleeping. Wakes the timer;
    /// [`Executor::tick`] applies what came due at once.
    #[cfg(any(test, feature = "testing"))]
    pub fn advance(&self, by: Duration) {
        self.offset_ms.fetch_add(millis(by), Ordering::Relaxed);
        self.timer.notify_one();
    }

    /// Restarts the queue after a daemon start: every running claim's lease
    /// is extended to at least one lease from now, so an executor that kept
    /// running across the restart goes on, and every open job's presence
    /// entry is published again. Then the timer starts.
    pub fn start(self: &Arc<Self>, shutdown: CancellationToken) {
        let now = self.now();
        match self.db.open_jobs() {
            Ok(jobs) => {
                for job in jobs {
                    let job = if job.state == JobState::Running {
                        self.transition(&job.id, |job| {
                            let lease = job.lease_ms.unwrap_or(millis(DEFAULT_LEASE));
                            let at_least = Timestamp(now.0.saturating_add(lease));
                            if job.state != JobState::Running
                                || job.lease_expires_at.is_some_and(|at| at >= at_least)
                            {
                                return Ok(false);
                            }
                            job.lease_expires_at = Some(at_least);
                            Ok(true)
                        })
                        .ok()
                        .flatten()
                        .unwrap_or(job)
                    } else {
                        job
                    };
                    self.publish(&job);
                }
            }
            Err(error) => tracing::warn!(
                error = %ErrorChain(&error),
                "cannot read the open executor jobs at start; their leases and presence wait for the timer"
            ),
        }
        let executor = Arc::clone(self);
        tokio::spawn(async move { executor.run_timer(shutdown).await });
    }

    async fn run_timer(self: Arc<Self>, shutdown: CancellationToken) {
        loop {
            let next = self.tick();
            let wait = next.map_or(TIMER_CEILING, |at| {
                Duration::from_millis(u64::try_from(at.0 - self.now().0).unwrap_or(0))
                    .min(TIMER_CEILING)
            });
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = self.timer.notified() => {}
                () = tokio::time::sleep(wait) => {}
            }
        }
    }

    /// Applies every lapse, deadline and queue timeout due now, through the
    /// same transactions the RPCs use, and returns when the next one is
    /// due. Called by the timer, and by tests after `Executor::advance`.
    pub fn tick(&self) -> Option<Timestamp> {
        let jobs = match self.db.open_jobs() {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::warn!(error = %ErrorChain(&error), "cannot read the open executor jobs");
                return None;
            }
        };
        let now = self.now();
        let mut next: Option<Timestamp> = None;
        for job in jobs {
            let job = if due_at(&job).is_some_and(|at| at <= now) {
                match self.transition(&job.id, |job| Ok(apply_due(job, self.now()))) {
                    Ok(Some(job)) => job,
                    Ok(None) => continue,
                    Err(error) => {
                        tracing::warn!(job_id = %job.id, error = %ErrorChain(&*error), "cannot apply a job's due transition");
                        continue;
                    }
                }
            } else {
                job
            };
            if let Some(at) = due_at(&job) {
                next = Some(next.map_or(at, |n| n.min(at)));
            }
        }
        next
    }

    /// Reads job `id` again in an immediate transaction, lets `change`
    /// change it, and writes it back when it says it changed. After the
    /// commit, publishes its presence entry, wakes waiting claims if it was
    /// queued again, and wakes the timer. Returns the job as written, or
    /// `None` when it changed nothing; `JOB_NOT_FOUND` when there is no
    /// such job.
    fn transition(
        &self,
        id: &str,
        change: impl FnOnce(&mut JobRow) -> Result<bool, ExecutorError>,
    ) -> Result<Option<JobRow>, Box<ExecutorError>> {
        let written = self.db.transact_then::<_, Box<ExecutorError>>(
            |tx| {
                let mut job = job_in(tx, id)?.ok_or_else(|| {
                    Box::new(ExecutorError::NotFound {
                        job_id: id.to_owned(),
                    })
                })?;
                let before = job.state;
                if !change(&mut job).map_err(Box::new)? {
                    return Ok(None);
                }
                update_job_in(tx, &job)?;
                Ok(Some((before, job)))
            },
            |written| {
                if let Some((before, job)) = written {
                    self.after(*before, job);
                }
            },
        )?;
        Ok(written.map(|(_, job)| job))
    }

    /// What follows a committed transition of `job` from `before`, called
    /// with the database's lock still held ([`Db::transact_then`]), so
    /// publishes happen in commit order.
    fn after(&self, before: JobState, job: &JobRow) {
        audit::transition(before, job);
        self.publish(job);
        if job.state == JobState::Queued {
            self.queued.notify_waiters();
        }
        self.timer.notify_one();
    }

    /// Claims the best matching queued job for `claimant`, waiting up to
    /// `wait` for one. Returns it running with the attempt's claim token,
    /// or `None` when nothing matched in time or the daemon is stopping.
    pub async fn claim(
        &self,
        claimant: &Claimant,
        wait: Duration,
        shutdown: &CancellationToken,
    ) -> Result<Option<(JobRow, String)>, ExecutorError> {
        // A waiting claim holds one of its remote token's request slots;
        // past half of them, a claim answers at once, so the token's
        // heartbeats and completions always find a slot.
        let waiting = Waiting::enter(&self.waiting, claimant.columns.token.as_deref());
        let wait = if waiting.is_some() {
            wait
        } else {
            Duration::ZERO
        };
        let until = tokio::time::Instant::now() + wait;
        loop {
            let notified = self.queued.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(claimed) = self.claim_now(claimant)? {
                return Ok(Some(claimed));
            }
            tokio::select! {
                () = notified => {}
                () = tokio::time::sleep_until(until) => return Ok(None),
                () = shutdown.cancelled() => return Ok(None),
            }
        }
    }

    /// One claim attempt: selection and the move to running in one
    /// transaction.
    fn claim_now(&self, claimant: &Claimant) -> Result<Option<(JobRow, String)>, ExecutorError> {
        let now = self.now();
        let claimed = self.db.transact_then(
            |tx| {
                let Some(mut job) = queued_jobs_in(tx)?
                    .into_iter()
                    .find(|job| now < job.queue_expires_at && claimant.matches(job))
                else {
                    return Ok::<_, ExecutorError>(None);
                };
                job.state = JobState::Running;
                job.attempt += 1;
                let token = new_claim_token(job.attempt);
                job.claim_digest = Some(claim_digest(&token).as_bytes().to_vec());
                job.claimed_at = Some(now);
                let lease = millis(claimant.lease);
                job.lease_ms = Some(lease);
                job.lease_expires_at = Some(Timestamp(now.0.saturating_add(lease)));
                job.deadline_at = Some(Timestamp(
                    now.0
                        .saturating_add(job.wall_time_ms)
                        .saturating_add(millis(DEADLINE_GRACE)),
                ));
                job.executor = Some(claimant.columns.clone());
                job.progress = None;
                update_job_in(tx, &job)?;
                Ok(Some((job, token)))
            },
            |claimed| {
                if let Some((job, _)) = claimed {
                    self.after(JobState::Queued, job);
                }
            },
        )?;
        Ok(claimed)
    }

    /// Cancels the job `id` (a full id), recording `agent` and `reason`. A
    /// cancelled job is returned as it is; a completed or expired one is
    /// `JOB_FINISHED`. `owner` is the remote caller's token, which must be
    /// the one that submitted it.
    pub fn cancel(
        &self,
        id: &str,
        owner: Option<&str>,
        agent: Option<&str>,
        reason: Option<&str>,
    ) -> Result<JobRow, ExecutorError> {
        let now = self.now();
        let written = self
            .transition(id, |job| {
                if let Some(token) = owner
                    && job.requested_by_token.as_deref() != Some(token)
                {
                    return Err(ExecutorError::NotOwned {
                        job_id: job.id.clone(),
                        claimed: false,
                    });
                }
                match job.state {
                    JobState::Cancelled => Ok(false),
                    JobState::Completed | JobState::Expired => Err(ExecutorError::Finished {
                        job_id: job.id.clone(),
                        state: job.state.as_str(),
                    }),
                    JobState::Queued | JobState::Running => {
                        if job.state == JobState::Running {
                            end_attempt_time(job, now);
                        }
                        job.state = JobState::Cancelled;
                        job.finished_at = Some(now);
                        job.lease_expires_at = None;
                        job.deadline_at = None;
                        job.cancelled_by_agent = agent.map(str::to_owned);
                        job.cancel_reason = reason.map(str::to_owned);
                        job.message = reason.map(str::to_owned);
                        Ok(true)
                    }
                }
            })
            .map_err(|error| *error)?;
        match written {
            Some(job) => Ok(job),
            None => self.db.job(id)?.ok_or_else(|| ExecutorError::NotFound {
                job_id: id.to_owned(),
            }),
        }
    }

    /// The job `id`, checked for a call through the remote token `caller`
    /// (none on the socket) before the call's evidence is verified: the
    /// cheap refusals first. The transaction that stores the call checks the
    /// owner again, and the claim token.
    pub fn check_claim(
        &self,
        id: &str,
        token: &str,
        caller: Option<&str>,
    ) -> Result<JobRow, ExecutorError> {
        let job = self.db.job(id)?.ok_or_else(|| ExecutorError::NotFound {
            job_id: id.to_owned(),
        })?;
        // A completion repeated after it succeeded goes on to its check
        // inside the transaction.
        if job.state == JobState::Completed && digest_matches(&job, token) {
            check_owner(&job, caller)?;
            return Ok(job);
        }
        let mut due = job.clone();
        apply_due(&mut due, self.now());
        check_call(&due, token, caller)?;
        Ok(job)
    }

    /// The evidence `job` accepted.
    pub fn evidence_of(&self, job: &JobRow) -> Result<Vec<JobEvidenceRow>, ExecutorError> {
        Ok(self.db.job_evidence(job.repo_id, &job.id)?)
    }

    /// Publishes `job`'s presence entry, or ends it when the job finished.
    fn publish(&self, job: &JobRow) {
        presence::publish(&self.presence, job, self.now());
    }
}

/// The claim check of an update or a completion: a token for an attempt
/// that is over, or for a job that is not running, is `JOB_NOT_CLAIMED`
/// whoever sends it, so an executor whose job lapsed and was claimed
/// elsewhere learns to stop; then `OwnClaim`; then the token itself.
fn check_call(job: &JobRow, token: &str, caller: Option<&str>) -> Result<(), ExecutorError> {
    let problem = claim_problem(job, token);
    let over = problem.is_some()
        && (job.state != JobState::Running
            || token_attempt(token).is_none_or(|attempt| attempt < job.attempt));
    if !over {
        check_owner(job, caller)?;
    }
    match problem {
        Some(state) => Err(ExecutorError::NotClaimed {
            job_id: job.id.clone(),
            state,
        }),
        None => Ok(()),
    }
}

/// The remote rules `OwnClaim`: through the remote listener, only the
/// token that claimed the job may update or complete it.
fn check_owner(job: &JobRow, caller: Option<&str>) -> Result<(), ExecutorError> {
    let Some(token) = caller else {
        return Ok(());
    };
    let holder = job.executor.as_ref().and_then(|e| e.token.as_deref());
    if holder != Some(token) {
        return Err(ExecutorError::NotOwned {
            job_id: job.id.clone(),
            claimed: true,
        });
    }
    Ok(())
}

#[cfg(test)]
fn placeholder() -> JobRow {
    JobRow {
        repo_id: "0000000000000000".parse().expect("a repository id"),
        id: String::new(),
        protocol: reef_api::EXECUTOR_PROTOCOL,
        tree_id: String::new(),
        commit_id: None,
        change_id: None,
        kinds: Vec::new(),
        targets: Vec::new(),
        requirements: Vec::new(),
        spec_digest: String::new(),
        wall_time_ms: 0,
        cpu_time_ms: None,
        priority: 0,
        state: JobState::Expired,
        outcome: None,
        end_reason: None,
        message: None,
        attempt: 0,
        max_attempts: 0,
        submitted_at: Timestamp(0),
        queue_expires_at: Timestamp(0),
        requested_by_agent: None,
        requested_by_token: None,
        claim_digest: None,
        claimed_at: None,
        lease_ms: None,
        lease_expires_at: None,
        deadline_at: None,
        executor: None,
        progress: None,
        usage_wall_ms: None,
        usage_cpu_ms: None,
        elapsed_ms: 0,
        finished_at: None,
        cancelled_by_agent: None,
        cancel_reason: None,
    }
}

/// When `job` next needs the timer: its queue expiry while queued; its
/// lease's or its deadline's, whichever is first, while running.
fn due_at(job: &JobRow) -> Option<Timestamp> {
    match job.state {
        JobState::Queued => Some(job.queue_expires_at),
        JobState::Running => match (job.lease_expires_at, job.deadline_at) {
            (Some(lease), Some(deadline)) => Some(lease.min(deadline)),
            (lease, deadline) => lease.or(deadline),
        },
        _ => None,
    }
}

/// Applies to `job` what is due at `now`: a queue timeout, a deadline, or
/// a lapsed lease. Returns whether it changed.
fn apply_due(job: &mut JobRow, now: Timestamp) -> bool {
    match job.state {
        JobState::Queued if now >= job.queue_expires_at => {
            expire(job, now, "queue_timeout");
            true
        }
        JobState::Running if job.deadline_at.is_some_and(|at| now >= at) => {
            end_attempt_time(job, now);
            expire(job, now, "deadline");
            true
        }
        JobState::Running if job.lease_expires_at.is_some_and(|at| now >= at) => {
            end_attempt(job, now);
            true
        }
        _ => false,
    }
}

/// Ends `job`'s running attempt, by a lapse or a release: queued again
/// with the next attempt's queue expiry, or expired when that was its
/// last attempt.
fn end_attempt(job: &mut JobRow, now: Timestamp) {
    end_attempt_time(job, now);
    if job.attempt >= job.max_attempts {
        expire(job, now, "attempts");
        return;
    }
    job.state = JobState::Queued;
    job.claim_digest = None;
    job.lease_expires_at = None;
    job.deadline_at = None;
    let lease = job.lease_ms.unwrap_or(millis(DEFAULT_LEASE));
    job.queue_expires_at = job
        .queue_expires_at
        .max(Timestamp(now.0.saturating_add(lease)));
}

/// Adds the running attempt's time, from its claim to `now`, to
/// `elapsed_ms`.
fn end_attempt_time(job: &mut JobRow, now: Timestamp) {
    if let Some(claimed) = job.claimed_at {
        job.elapsed_ms = job.elapsed_ms.saturating_add((now.0 - claimed.0).max(0));
    }
}

/// Ends `job` as expired for `reason`.
fn expire(job: &mut JobRow, now: Timestamp, reason: &str) {
    job.state = JobState::Expired;
    job.end_reason = Some(reason.to_owned());
    job.finished_at = Some(now);
    job.lease_expires_at = None;
    job.deadline_at = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_and_prefixes() {
        assert_eq!(
            full_id(" job_00AB000000000001 ").unwrap(),
            "job_00ab000000000001"
        );
        assert!(full_id("job_00ab").is_err());
        assert!(full_id("00ab000000000001").is_err());
        assert_eq!(id_prefix("job_00AB").unwrap(), "00ab");
        assert!(id_prefix("job_0").is_err());
        assert!(id_prefix("job_zzzz").is_err());
        assert_eq!(presence_key("job_0000000000000010"), 16);
    }

    #[test]
    fn waiting_claims_are_capped_per_token() {
        let map = Mutex::new(HashMap::new());
        let held: Vec<_> = (0..MAX_WAITING_PER_TOKEN)
            .map(|_| Waiting::enter(&map, Some("tok")).unwrap())
            .collect();
        assert!(Waiting::enter(&map, Some("tok")).is_none(), "past the cap");
        assert!(Waiting::enter(&map, Some("other")).is_some(), "per token");
        assert!(
            Waiting::enter(&map, None).is_some(),
            "the socket has no cap"
        );
        drop(held);
        assert!(Waiting::enter(&map, Some("tok")).is_some());
        assert!(
            map.lock().unwrap().is_empty(),
            "counts go with their claims"
        );
    }

    #[test]
    fn claim_tokens_name_their_attempt_and_check_in_full() {
        let mut job = placeholder();
        job.state = JobState::Running;
        job.attempt = 2;
        let token = new_claim_token(2);
        job.claim_digest = Some(claim_digest(&token).as_bytes().to_vec());
        assert_eq!(claim_problem(&job, &token), None);
        assert_eq!(
            claim_problem(&job, &new_claim_token(2)),
            Some("wrong_token")
        );
        assert_eq!(claim_problem(&job, &new_claim_token(1)), Some("lapsed"));
        assert_eq!(claim_problem(&job, "garbage"), Some("wrong_token"));
        let mut never = placeholder();
        never.state = JobState::Queued;
        assert_eq!(
            claim_problem(&never, &new_claim_token(0)),
            Some("wrong_token"),
            "nobody ever held a job not yet claimed"
        );
        job.state = JobState::Queued;
        assert_eq!(claim_problem(&job, &token), Some("lapsed"));
        job.state = JobState::Cancelled;
        assert_eq!(claim_problem(&job, &token), Some("cancelled"));
    }
}
