//! Automatic landing and evidence requests (R3.45, ADR 0081;
//! `docs/landing.md` "Automatic landing").
//!
//! One task keeps an **automatic status** for every open proposal (a change
//! `proposed` for a destination): what the daemon is doing about it and
//! what it waits for. A proposal is judged again only when its **evaluation
//! key** changes: a SHA-256 over what the gate reads that can be read
//! cheaply (the stack's commits and trees, the blob of `reef-policy.toml`
//! at the destination, the evidence ids on those trees, the change's
//! provenance), with the destination's head only when the judgment depends
//! on it. Events the key cannot see (transitions, revocations, pruning)
//! mark proposals instead. Statuses live in memory; a start judges every
//! open proposal once.
//!
//! The judgment is the gate's own ([`super::decide`]), as a dry run, over
//! each stack's head: a stale status can delay a landing, never admit one,
//! since the gate's writes check again and the worker judges again. From
//! it the task asks executors for the `[[require]]` pairs missing on each
//! change's own tree (under `[landing] checks = "batch"`), and, when the
//! destination says `[landing] automatic = true`, queues the stack's
//! passing prefix through the gate's writes, unless its first unpassed
//! change only waits for requested evidence.
//!
//! Bounds: one task for every repository, so one judgment at a time
//! daemon-wide; at most [`PER_PASS`] judgments per pass, one pass per
//! repository every two seconds; at most [`MAX_OPEN_JOBS`] open jobs per
//! repository; nothing asked for while no executor answers.

mod judge;

pub(crate) use judge::SIGNOFF_NEED;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use futures::FutureExt as _;
use reef_api::v1::{PresenceEndReason, PresenceKind};
use reef_lib::error::ErrorChain;
use reef_lib::jj::land::{self, LandError, StackMember};
use reef_lib::jj::{ChangeIdHex, CommitIdHex, files};
use reef_policy::{Checks, Need};
use tokio::sync::Notify;

use super::Reason;
use super::batch::AGENT;
use crate::daemon::AppState;
use crate::db::{JobState, LifecycleRow};
use crate::model::{RepoId, Timestamp};
use crate::policy::Loaded;
use crate::presence::{DaemonKey, PresenceHub, Report};
use crate::service::policy as policy_input;
use judge::{Budget, Judging};

/// The priority of the jobs asked for proposals: below a batch's
/// ([`super::BATCH_PRIORITY`]), so landings finish first (R3.21 replaces
/// both).
pub const PROPOSAL_PRIORITY: i32 = 500;

/// The most open jobs the service keeps asked for in one repository.
pub const MAX_OPEN_JOBS: usize = 32;

/// The most stacks judged in one pass over a repository.
pub const PER_PASS: usize = 16;

/// How long the task waits after a wake-up, so a burst is looked at once.
const SETTLE: Duration = Duration::from_millis(500);

/// The shortest time between two passes over one repository.
const GAP: Duration = Duration::from_secs(2);

/// How often the task looks again while something waits on a job or a
/// retry, in case a wake-up was missed.
const TICK: Duration = Duration::from_secs(30);

/// A judgment older than this is made again at the next pass, as a
/// backstop for an input the key missed.
const STALE: Duration = Duration::from_hours(1);

/// How long after a `busy` return a proposal is queued again.
const BUSY_AFTER: Duration = Duration::from_mins(1);

/// How long before a status held back by something passing (a full job
/// slot, an error) is judged again.
const RETRY: Duration = Duration::from_secs(30);

/// How many `busy` returns under one key before it stops.
const MAX_BUSY: usize = 3;

/// How many needs a status keeps.
const MAX_MISSING: usize = 20;

/// How many times a job for one pair ends with no verdict before the pair
/// is the operator's.
const MAX_INFRASTRUCTURE: usize = 2;

/// Where a proposal stands.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    /// The destination neither asks for evidence nor lands automatically,
    /// or has no valid policy.
    Off,
    /// Not judged since the daemon started or since it changed.
    Pending,
    /// Evidence it lacks was asked for; jobs are open.
    Waiting,
    /// The gate allows it, and the destination does not land automatically.
    Ready,
    /// Queued for landing.
    Queued,
    /// Blocked by something no job provides.
    Needs,
    /// The jobs it needs expired unclaimed, and no executor claimed since.
    NoExecutor,
    /// A landing the service queued returned, and nothing changed since.
    Returned,
}

impl State {
    /// The name used in logs, the CLI and documents.
    pub fn as_str(self) -> &'static str {
        match self {
            State::Off => "off",
            State::Pending => "pending",
            State::Waiting => "waiting",
            State::Ready => "ready",
            State::Queued => "queued",
            State::Needs => "needs",
            State::NoExecutor => "no_executor",
            State::Returned => "returned",
        }
    }
}

/// One proposal's automatic status.
#[derive(Clone, Debug)]
pub struct Status {
    /// Where it stands.
    pub state: State,
    /// What the gate said, or what it waits for.
    pub message: String,
    /// The pairs it waits for, or the needs that block it; at most 20.
    pub missing: Vec<Need>,
    /// How many there are.
    pub missing_total: usize,
    /// The jobs it waits on.
    pub jobs: Vec<String>,
    /// Its landing, when queued or returned.
    pub landing_id: Option<String>,
    /// The gate's or the landing's reason.
    pub reason: Option<String>,
    /// A command line that shows or fixes it, as an argv.
    pub next: Vec<String>,
    /// When it was judged.
    pub evaluated_at: Timestamp,
    /// The commit its proposal recorded.
    pub commit: String,
    /// The bookmark it is proposed for.
    pub destination: String,
    /// Whether that destination's policy says `[landing] automatic = true`.
    pub automatic: bool,
    /// The key it was judged under.
    key: String,
    /// Whether its judgment depended on the destination's head.
    sensitive: bool,
    /// When it is judged again whatever its key: a returned landing that
    /// may be queued again, a job slot that may have freed, an error that
    /// may have passed.
    retry_at: Option<Instant>,
    /// Its tree, whose jobs it may share.
    tree: Option<String>,
}

impl Status {
    /// A returned landing the service queues again by itself.
    pub fn retrying(&self) -> bool {
        self.state == State::Returned && self.retry_at.is_some()
    }

    fn new(state: State, commit: &str) -> Status {
        Status {
            state,
            message: String::new(),
            missing: Vec::new(),
            missing_total: 0,
            jobs: Vec::new(),
            landing_id: None,
            reason: None,
            next: Vec::new(),
            evaluated_at: Timestamp::now(),
            commit: commit.to_owned(),
            destination: String::new(),
            automatic: false,
            key: String::new(),
            sensitive: false,
            retry_at: None,
            tree: None,
        }
    }

    /// Whether the service will land the proposal of `commit` onto
    /// `destination` with no request (#262: `reef here`'s step is then
    /// `wait`). Only while it may still act: once it needs something, a
    /// landing returned, or no executor claims, nothing lands until someone
    /// acts. A status from an earlier proposal of the change, onto another
    /// destination or of another commit, says nothing about this one.
    pub fn lands_unasked(&self, destination: &str, commit: &str) -> bool {
        self.automatic
            && self.destination == destination
            && self.commit == commit
            && matches!(self.state, State::Pending | State::Waiting | State::Queued)
    }

    fn with_missing(mut self, needs: Vec<Need>) -> Status {
        self.missing_total = needs.len();
        self.missing = needs.into_iter().take(MAX_MISSING).collect();
        self
    }
}

/// The presence key of `change`'s status: its repository and the first 64
/// bits of its change id.
fn presence_key(repo: RepoId, change: &str) -> DaemonKey {
    let bits = change
        .get(..16)
        .and_then(|prefix| u64::from_str_radix(prefix, 16).ok())
        .unwrap_or(0);
    DaemonKey::Automatic(repo, bits)
}

/// The service's state; one in [`AppState`].
#[derive(Debug, Default)]
pub struct Automatic {
    inner: Mutex<Inner>,
    wake: Notify,
    changed: Notify,
    evaluations: AtomicU64,
}

#[derive(Debug, Default)]
struct Inner {
    statuses: HashMap<RepoId, HashMap<String, Status>>,
    dirty: HashSet<RepoId>,
    marked: HashMap<RepoId, HashSet<String>>,
    all: HashSet<RepoId>,
    passed: HashMap<RepoId, Instant>,
    versions: HashMap<RepoId, u64>,
}

impl Automatic {
    fn with<T>(&self, f: impl FnOnce(&mut Inner) -> T) -> T {
        f(&mut self.inner.lock().unwrap_or_else(PoisonError::into_inner))
    }

    /// Something in `repo` changed that may move a proposal's key: the task
    /// looks at its proposals again.
    pub fn wake(&self, repo: RepoId) {
        self.with(|inner| inner.dirty.insert(repo));
        self.wake.notify_one();
    }

    /// `changes` must be judged again whatever their keys say: a
    /// transition changed their stack.
    pub fn mark(&self, repo: RepoId, changes: impl IntoIterator<Item = String>) {
        self.with(|inner| {
            inner.marked.entry(repo).or_default().extend(changes);
            inner.dirty.insert(repo);
        });
        self.wake.notify_one();
    }

    /// Every proposal of `repo` must be judged again: something outside
    /// the keys changed (a revocation, pruned evidence or artifacts).
    pub fn mark_all(&self, repo: RepoId) {
        self.with(|inner| {
            inner.all.insert(repo);
            inner.dirty.insert(repo);
        });
        self.wake.notify_one();
    }

    /// `change`'s status, if it is an open proposal the task has seen.
    pub fn status(&self, repo: RepoId, change: &str) -> Option<Status> {
        self.with(|inner| {
            inner
                .statuses
                .get(&repo)
                .and_then(|statuses| statuses.get(change))
                .cloned()
        })
    }

    /// Notified whenever a status changes.
    pub fn changed(&self) -> &Notify {
        &self.changed
    }

    /// How many judgments the task has made since the daemon started.
    pub fn evaluations(&self) -> u64 {
        self.evaluations.load(Ordering::Relaxed)
    }

    /// Replaces `repo`'s statuses, and returns those that moved (new, or
    /// another key or state), for the agents' instructions. Publishes the
    /// `AUTOMATIC` entry of each judged status that moved or whose entry
    /// lapsed, and ends those of proposals gone or now `OFF` (R3.33, #362).
    /// A status first judged `OFF` is published and ended at once: the
    /// inbox's words for it differ from an unjudged one's, and presence is
    /// how the inbox learns that it moved (#387 review).
    fn set(
        &self,
        hub: &PresenceHub,
        repo: RepoId,
        statuses: &HashMap<String, Status>,
    ) -> Vec<(String, Status)> {
        let before = self
            .with(|inner| inner.statuses.insert(repo, statuses.clone()))
            .unwrap_or_default();
        self.changed.notify_waiters();
        let moved: Vec<(String, Status)> = statuses
            .iter()
            .filter(|(change, status)| {
                before
                    .get(*change)
                    .is_none_or(|old| old.key != status.key || old.state != status.state)
            })
            .map(|(change, status)| (change.clone(), status.clone()))
            .collect();
        let live: HashSet<DaemonKey> = hub.automatic_keys(repo).into_iter().collect();
        let judged: HashSet<DaemonKey> = statuses
            .keys()
            .map(|change| presence_key(repo, change))
            .collect();
        for key in live.difference(&judged) {
            hub.end_daemon(*key, PresenceEndReason::Cleared);
        }
        for (change, status) in statuses {
            let key = presence_key(repo, change);
            let had = live.contains(&key);
            let changed = moved.iter().any(|(name, _)| name == change);
            let off = status.state == State::Off;
            let publish = if off {
                changed && !had
            } else {
                changed || !had
            };
            if publish {
                let mut message = super::messages::automatic_message(status);
                message.missing.clear();
                hub.publish_daemon(key, |_| Report {
                    repo: Some(repo),
                    change_id: change.clone(),
                    kind: PresenceKind::Automatic,
                    automatic: Some(message),
                    ttl: crate::presence::STATE_TTL,
                    ..Report::default()
                });
            }
            if off && (had || publish) {
                hub.end_daemon(key, PresenceEndReason::Cleared);
            }
        }
        moved
    }
}

/// Starts the task; it stops at shutdown.
pub(crate) fn start(state: &Arc<AppState>) {
    let weak: Weak<AppState> = Arc::downgrade(state);
    let shutdown = state.shutdown.clone();
    let mut ops = state.events.changes();
    // Every repository is looked at once at start.
    for repo in state.repos.ids() {
        state.automatic.with(|inner| inner.dirty.insert(repo));
    }
    tokio::spawn(async move {
        loop {
            let Some(state) = weak.upgrade() else {
                return;
            };
            let tick = run(&state).await;
            let mut ticked = false;
            // Every wake-up keeps a permit (`notify_one`), so one that
            // fires during a pass or the settle is seen at the next wait.
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = state.automatic.wake.notified() => {}
                () = state.executor.touched().notified() => {}
                () = state.provenance.touched_notify.notified() => {}
                changed = ops.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = tokio::time::sleep(tick) => ticked = true,
            }
            collect(&state, ticked);
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(SETTLE) => {}
            }
            collect(&state, false);
        }
    });
}

/// Marks dirty the repositories something happened in since the last
/// look: their jobs, their provenance, their operation log; every
/// proposal after a revocation; every repository when the tick fired.
fn collect(state: &AppState, ticked: bool) {
    let jobs = state.executor.take_touched();
    let (recorded, revoked) = {
        let mut touched = state
            .provenance
            .touched
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        (
            std::mem::take(&mut touched.repos),
            std::mem::take(&mut touched.revoked),
        )
    };
    let ids = state.repos.ids();
    let versions: Vec<(RepoId, Option<u64>)> = ids
        .iter()
        .map(|repo| (*repo, state.events.view(*repo).map(|view| view.version)))
        .collect();
    state.automatic.with(|inner| {
        inner.dirty.extend(jobs);
        inner.dirty.extend(recorded);
        for (repo, version) in versions {
            if let Some(version) = version
                && inner.versions.insert(repo, version) != Some(version)
            {
                inner.dirty.insert(repo);
            }
            if ticked {
                inner.dirty.insert(repo);
            }
            if revoked {
                inner.all.insert(repo);
                inner.dirty.insert(repo);
            }
        }
    });
}

/// One pass over every dirty repository whose last pass is [`GAP`] old.
/// Returns how long the task may sleep with no wake-up: until a dirty
/// repository is due, [`TICK`] while anything waits on a job or a retry,
/// else [`STALE`].
async fn run(state: &Arc<AppState>) -> Duration {
    let now = Instant::now();
    let due: Vec<RepoId> = state.automatic.with(|inner| {
        let due: Vec<RepoId> = inner
            .dirty
            .iter()
            .copied()
            .filter(|repo| {
                inner
                    .passed
                    .get(repo)
                    .is_none_or(|at| now.duration_since(*at) >= GAP)
            })
            .collect();
        for repo in &due {
            inner.dirty.remove(repo);
            inner.passed.insert(*repo, now);
        }
        due
    });
    for repo in due {
        match pass(state, repo).await {
            Ok(true) => {
                state.automatic.with(|inner| inner.dirty.insert(repo));
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(repo = %repo, error, "cannot look at proposals for automatic landing");
            }
        }
    }
    state.automatic.with(|inner| {
        let now = Instant::now();
        let due = inner
            .dirty
            .iter()
            .map(|repo| {
                inner.passed.get(repo).map_or(Duration::ZERO, |at| {
                    GAP.saturating_sub(now.duration_since(*at))
                })
            })
            .min();
        let waiting = inner.statuses.values().flatten().any(|(_, status)| {
            matches!(
                status.state,
                State::Waiting | State::NoExecutor | State::Queued
            ) || status.retry_at.is_some()
        });
        match due {
            Some(due) => due,
            None if waiting => TICK,
            // The backstop for an input no key or wake-up saw.
            None => STALE,
        }
    })
}

/// What the repository thread reads for one destination.
struct Destination {
    name: String,
    head: Option<CommitIdHex>,
    policy: Option<policy_input::PolicyFile>,
    /// Each proposal's stack, by its change, or why it could not be read.
    stacks: HashMap<String, Result<Vec<StackMember>, String>>,
}

/// What the destination's policy says about automatic work.
#[derive(Clone, Copy)]
struct Settings {
    requests: bool,
    automatic: bool,
    reads_destination: bool,
}

fn settings(state: &AppState, policy: Option<&policy_input::PolicyFile>) -> Option<Settings> {
    let bytes = policy?.bytes.as_ref().ok()?;
    match &*state.policy.load(bytes) {
        Loaded::Policy(policy) => Some(Settings {
            requests: policy.landing().checks == Checks::Batch,
            automatic: policy.landing().automatic,
            reads_destination: policy.reads_destination(),
        }),
        Loaded::Invalid(_) => None,
    }
}

/// Reads every proposal's stack and every destination's policy, on the
/// repository thread.
async fn read(
    state: &AppState,
    repo: RepoId,
    proposals: &[LifecycleRow],
) -> Result<Vec<Destination>, String> {
    let mut by_destination: HashMap<String, Vec<(String, String)>> = HashMap::new();
    for row in proposals {
        let destination = row.destination.clone().unwrap_or_default();
        by_destination
            .entry(destination)
            .or_default()
            .push((row.change_id.clone(), row.commit_id.clone()));
    }
    let handle = state
        .manager
        .handle(repo)
        .map_err(|error| ErrorChain(&error).to_string())?;
    handle
        .call_async(move |head| {
            async move {
                let repo = head
                    .refresh()
                    .await
                    .map_err(|error| ErrorChain(&error).to_string())?;
                let mut out = Vec::with_capacity(by_destination.len());
                for (name, changes) in by_destination {
                    let onto = land::destination_head(&repo, &name).ok();
                    let mut policy = None;
                    if let Some(onto) = &onto {
                        let tree = files::commit_tree(&repo, onto)
                            .await
                            .map_err(|error| ErrorChain(&error).to_string())?;
                        policy = policy_input::policy_file(&tree)
                            .await
                            .map_err(|status| status.message().to_owned())?;
                    }
                    // Each change's visible commit, so an amendment after
                    // proposing changes the stack read and its key.
                    let ids: Vec<String> =
                        changes.iter().map(|(change, _)| change.clone()).collect();
                    let visible = files::visible_commits_of_changes(&repo, &ids)
                        .await
                        .map_err(|error| ErrorChain(&error).to_string())?;
                    let mut stacks = HashMap::new();
                    for (change, recorded) in changes {
                        let commit = match visible.get(&change).map(Vec::as_slice) {
                            Some([one]) => one.clone(),
                            _ => recorded,
                        };
                        let found = match (&onto, commit.parse::<CommitIdHex>()) {
                            (Some(onto), Ok(commit)) => land::stack(&repo, &commit, onto)
                                .await
                                .map_err(|error: LandError| ErrorChain(&error).to_string()),
                            (None, _) => Err(format!("{name} does not exist or is conflicted")),
                            (_, Err(_)) => Err(format!("{commit} is not a commit id")),
                        };
                        stacks.insert(change, found);
                    }
                    out.push(Destination {
                        name,
                        head: onto,
                        policy,
                        stacks,
                    });
                }
                Ok::<_, String>(out)
            }
            .boxed_local()
        })
        .await
        .map_err(|error| ErrorChain(&error).to_string())?
}

/// The evaluation key of `members` (bottom first) under `policy` at
/// `destination`.
fn key(
    state: &AppState,
    repo: RepoId,
    members: &[StackMember],
    policy: Option<&str>,
    destination: Option<&str>,
) -> Result<String, String> {
    let mut hasher: Vec<u8> = Vec::with_capacity(4096);
    hasher.extend_from_slice(b"reef automatic key 1\n");
    hasher.extend_from_slice(policy.unwrap_or("-").as_bytes());
    hasher.extend_from_slice(b"\n");
    if let Some(destination) = destination {
        hasher.extend_from_slice(destination.as_bytes());
    }
    hasher.extend_from_slice(b"\n");
    for member in members {
        hasher.extend_from_slice(member.change.as_str().as_bytes());
        hasher.extend_from_slice(b" ");
        hasher.extend_from_slice(member.commit.as_str().as_bytes());
        hasher.extend_from_slice(b"\n");
        if let Some(tree) = &member.tree {
            let ids = state
                .db
                .evidence_ids_on_tree(repo, tree)
                .map_err(|error| ErrorChain(&error).to_string())?;
            for id in ids {
                hasher.extend_from_slice(id.as_bytes());
            }
        }
        hasher.extend_from_slice(b"\n");
        let (count, newest) = state
            .db
            .provenance_mark(repo, member.change.as_str())
            .map_err(|error| ErrorChain(&error).to_string())?;
        hasher.extend_from_slice(format!("{count} {newest}\n").as_bytes());
    }
    Ok(reef_policy::sha256_hex(&hasher))
}

/// The proposals whose stack heads are judged: those that are no other
/// proposal's ancestor in its stack.
fn heads(destination: &Destination) -> Vec<(String, Vec<StackMember>)> {
    let mut below: HashSet<String> = HashSet::new();
    for found in destination.stacks.values().flatten() {
        if let Some((_, under)) = found.split_last() {
            below.extend(under.iter().map(|member| member.change.as_str().to_owned()));
        }
    }
    let mut heads = Vec::new();
    for (change, found) in &destination.stacks {
        if below.contains(change) {
            continue;
        }
        if let Ok(members) = found {
            heads.push((change.clone(), members.clone()));
        }
    }
    heads
}

/// One pass over `repo`'s proposals. Returns whether there is more to do.
#[expect(clippy::too_many_lines, reason = "the pass's steps in order")]
async fn pass(state: &Arc<AppState>, repo: RepoId) -> Result<bool, String> {
    // A dependent whose dependency was just rewritten is carried before it
    // is judged (R3.11), so a clean one is never told to propose again.
    if let Err(error) = crate::lifecycle::carry::carry(state, repo, None).await {
        tracing::warn!(%repo, error = %ErrorChain(&error), "cannot carry proposals before judging them");
    }
    let proposals = state
        .db
        .open_proposals(repo)
        .map_err(|error| ErrorChain(&error).to_string())?;
    let (marked, all) = state.automatic.with(|inner| {
        (
            inner.marked.remove(&repo).unwrap_or_default(),
            inner.all.remove(&repo),
        )
    });
    let before: HashMap<String, Status> = state
        .automatic
        .with(|inner| inner.statuses.get(&repo).cloned())
        .unwrap_or_default();
    let open: HashSet<&str> = proposals.iter().map(|row| row.change_id.as_str()).collect();
    // Trees an open proposal still needs, so a shared job is kept (S-226).
    let needed: HashSet<&str> = before
        .iter()
        .filter(|(change, _)| open.contains(change.as_str()))
        .filter_map(|(_, status)| status.tree.as_deref())
        .collect();
    // Proposals that left review take their queued jobs with them.
    let gone: Vec<&Status> = before
        .iter()
        .filter(|(change, status)| {
            !open.contains(change.as_str())
                || proposals
                    .iter()
                    .any(|row| &row.change_id == *change && row.commit_id != status.commit)
        })
        .map(|(_, status)| status)
        .collect();
    if !gone.is_empty() {
        cancel_unneeded(state, repo, &gone, &proposals, &needed);
    }
    if proposals.is_empty() {
        state.automatic.set(&state.presence, repo, &HashMap::new());
        return Ok(false);
    }
    let destinations = read(state, repo, &proposals).await?;
    let (claimed, _) = state
        .db
        .executor_seen(repo)
        .map_err(|error| ErrorChain(&error).to_string())?;
    // Marks of the stacks this pass leaves for the next one.
    let mut leftover: Vec<String> = Vec::new();
    let mut statuses: HashMap<String, Status> = HashMap::new();
    let mut judged = 0usize;
    let mut more = false;
    let mut budget = Budget::new(state, repo, &proposals);
    // Each proposal's destination, and whether it lands automatically.
    let mut onto: HashMap<&str, (&str, bool)> = HashMap::new();
    for destination in &destinations {
        let settings = settings(state, destination.policy.as_ref());
        let automatic = settings.is_some_and(|s| s.automatic);
        for change in destination.stacks.keys() {
            onto.insert(change, (&destination.name, automatic));
        }
        let blob = destination
            .policy
            .as_ref()
            .map(|file| file.blob_id.as_str());
        // Stack read errors leave the proposal for the gate to explain.
        for (change, found) in &destination.stacks {
            if let Err(error) = found {
                let commit = commit_of(&proposals, change);
                let mut status = Status::new(State::Needs, &commit);
                status.message.clone_from(error);
                status.reason = Some(Reason::Destination.as_str().to_owned());
                statuses.insert(change.clone(), status);
            }
        }
        let Some(settings) = settings.filter(|s| s.requests || s.automatic) else {
            for change in destination.stacks.keys() {
                let commit = commit_of(&proposals, change);
                let mut status = Status::new(State::Off, &commit);
                status.message = format!("{} lands only on request: `reef land`", destination.name);
                statuses.entry(change.clone()).or_insert(status);
            }
            continue;
        };
        let mut heads = heads(destination);
        // The longest waiting proposal first.
        heads.sort_by_key(|(change, _)| {
            proposals
                .iter()
                .position(|row| &row.change_id == change)
                .unwrap_or(usize::MAX)
        });
        for (head, members) in heads {
            let previous = before.get(&head);
            let sensitive =
                settings.reads_destination || previous.is_some_and(|status| status.sensitive);
            let key = key(
                state,
                repo,
                &members,
                blob,
                sensitive
                    .then(|| destination.head.as_ref().map(CommitIdHex::as_str))
                    .flatten(),
            )?;
            let names: Vec<&str> = members
                .iter()
                .map(|member| member.change.as_str())
                .collect();
            if !all
                && !names.iter().any(|name| marked.contains(*name))
                && let Some(previous) = previous
                && !due(state, previous, &key, claimed)
            {
                for name in &names {
                    if let Some(status) = before.get(*name) {
                        statuses.insert((*name).to_owned(), status.clone());
                    }
                }
                continue;
            }
            if judged >= PER_PASS {
                more = true;
                if all || names.iter().any(|name| marked.contains(*name)) {
                    leftover.push(head.clone());
                }
                for name in &names {
                    let commit = commit_of(&proposals, name);
                    statuses.entry((*name).to_owned()).or_insert_with(|| {
                        before
                            .get(*name)
                            .cloned()
                            .unwrap_or_else(|| Status::new(State::Pending, &commit))
                    });
                }
                continue;
            }
            judged += 1;
            let mut judgment = Judging {
                state,
                repo,
                destination: &destination.name,
                settings,
                key: &key,
                claimed,
            };
            let found = judgment.judge(&head, &members, &mut budget).await;
            for (name, status) in found {
                statuses.insert(name, status);
            }
        }
    }
    if !leftover.is_empty() {
        state.automatic.mark(repo, leftover);
    }
    // Every open proposal has a status, if only pending.
    for row in &proposals {
        statuses
            .entry(row.change_id.clone())
            .or_insert_with(|| Status::new(State::Pending, &row.commit_id));
    }
    for (change, status) in &mut statuses {
        let (destination, automatic) = onto.get(change.as_str()).copied().unwrap_or_default();
        destination.clone_into(&mut status.destination);
        status.automatic = automatic;
    }
    let instruct = state.automatic.set(&state.presence, repo, &statuses);
    for (change, status) in instruct {
        if status.automatic {
            super::instruct::send(state, repo, &change, &status).await;
        }
    }
    Ok(more)
}

fn commit_of(proposals: &[LifecycleRow], change: &str) -> String {
    proposals
        .iter()
        .find(|row| row.change_id == change)
        .map(|row| row.commit_id.clone())
        .unwrap_or_default()
}

/// Whether `previous`, judged under another or the same `key`, is due
/// again.
fn due(state: &AppState, previous: &Status, key: &str, claimed: Option<Timestamp>) -> bool {
    // A pending status keeps its key only with a time to try again.
    if previous.key != key {
        return true;
    }
    if Timestamp::now().0 - previous.evaluated_at.0
        > i64::try_from(STALE.as_millis()).unwrap_or(i64::MAX)
    {
        return true;
    }
    if previous.retry_at.is_some_and(|at| at <= Instant::now()) {
        return true;
    }
    match previous.state {
        // An executor came back.
        State::NoExecutor | State::Returned => {
            claimed.is_some_and(|claimed| claimed.0 > previous.evaluated_at.0)
        }
        // A job it waits on ended (evidence moves the key; an end with no
        // verdict does not).
        State::Waiting => previous.jobs.iter().any(|id| {
            state
                .db
                .job(id)
                .ok()
                .flatten()
                .is_none_or(|job| !job.state.is_open())
        }),
        _ => false,
    }
}

/// The revision naming `change` (hex) for the gate: jj's reverse hex.
pub(crate) fn change_revision(change: &str) -> String {
    change
        .parse::<ChangeIdHex>()
        .map_or_else(|_| change.to_owned(), |id| id.reverse_hex())
}

/// A change id cut to twelve characters, for messages.
pub(crate) fn short(change: &str) -> String {
    change_revision(change).chars().take(12).collect()
}

/// Cancels the jobs the service asked for `gone`'s proposals that are
/// still queued and that no open proposal (by commit or by tree, `needed`)
/// or open batch needs (S-226): running jobs finish, and their evidence
/// stays with the tree.
fn cancel_unneeded(
    state: &AppState,
    repo: RepoId,
    gone: &[&Status],
    open: &[LifecycleRow],
    needed: &HashSet<&str>,
) {
    let commits: HashSet<&str> = open.iter().map(|row| row.commit_id.as_str()).collect();
    for status in gone {
        for id in &status.jobs {
            let Ok(Some(job)) = state.db.job(id) else {
                continue;
            };
            if job.state != JobState::Queued
                || job.requested_by_agent.as_deref() != Some(AGENT)
                || job.commit_id.as_deref() != Some(status.commit.as_str())
                || commits.contains(status.commit.as_str())
                || needed.contains(job.tree_id.as_str())
                || state.db.job_in_open_trial(repo, id).unwrap_or(true)
            {
                continue;
            }
            match state
                .executor
                .cancel(id, None, Some(AGENT), Some("its proposal left review"))
            {
                Ok(_) => tracing::info!(repo = %repo, job_id = id, "cancelled a proposal's job"),
                Err(error) => {
                    tracing::debug!(repo = %repo, job_id = id, error = %ErrorChain(&error), "cannot cancel a proposal's job");
                }
            }
        }
    }
}

/// Waits, within `deadline`, while `change`'s status is `waiting` (R3.45:
/// `reef land` waits for the evidence the service asked for). With
/// `evaluate`, a change not judged since the request arrived is judged
/// first. Returns the status when it still waits at the end.
pub(crate) async fn wait_for_evidence(
    state: &AppState,
    repo: RepoId,
    change: &str,
    deadline: Instant,
    evaluate: bool,
) -> Option<Status> {
    let proposed = state
        .db
        .lifecycle_row(repo, change)
        .ok()
        .flatten()
        .is_some_and(|row| row.state == "proposed");
    if !proposed {
        return None;
    }
    let asked = Timestamp::now();
    // A status judged and settled answers at once; only a change not yet
    // judged, or waiting, is worth the wait.
    let evaluate = evaluate
        && state
            .automatic
            .status(repo, change)
            .is_none_or(|status| matches!(status.state, State::Pending | State::Waiting));
    if evaluate {
        state.automatic.mark(repo, [change.to_owned()]);
    } else if state
        .automatic
        .status(repo, change)
        .is_none_or(|status| status.state != State::Waiting)
    {
        return None;
    }
    loop {
        let changed = state.automatic.changed().notified();
        let status = state.automatic.status(repo, change);
        let judged = status
            .as_ref()
            .is_some_and(|status| !evaluate || status.evaluated_at.0 >= asked.0);
        let waits = status
            .as_ref()
            .is_some_and(|status| matches!(status.state, State::Waiting | State::Pending));
        let now = Instant::now();
        if judged && !waits {
            return None;
        }
        if now >= deadline {
            return status.filter(|status| status.state == State::Waiting);
        }
        if !judged
            && status
                .as_ref()
                .is_some_and(|status| status.state == State::Off)
        {
            return None;
        }
        let _ = tokio::time::timeout(deadline - now, changed).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presence::PresenceFilter;
    use crate::testing::TestDaemon;

    // #387 review: the inbox reads a proposal's words again on its
    // `AUTOMATIC` presence, so a status first judged `OFF` is announced.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_status_first_judged_off_is_announced_then_ended() {
        // A repository the daemon's own task never judges.
        let daemon = TestDaemon::start().await;
        let state = daemon.handle.state();
        let repo: RepoId = "aaaa000000000001".parse().unwrap();
        let change = "a".repeat(32);
        let (_, mut changes) = state.presence.subscribe(&PresenceFilter::default());
        let statuses = HashMap::from([(change.clone(), Status::new(State::Off, "c1"))]);
        state.automatic.set(&state.presence, repo, &statuses);
        let mut seen = Vec::new();
        while let Ok(seen_change) = changes.try_recv() {
            if seen_change.presence.change_id == change {
                seen.push(seen_change.end.is_some());
            }
        }
        assert_eq!(seen, [false, true], "published, then ended");
        assert!(
            state
                .presence
                .current(&PresenceFilter::default())
                .is_empty()
        );
        // Judged `OFF` again, it says nothing.
        state.automatic.set(&state.presence, repo, &statuses);
        assert!(changes.try_recv().is_err());
    }

    #[test]
    fn lands_unasked_only_while_the_service_may_act_on_this_proposal() {
        let status = |state, automatic| Status {
            destination: "main".to_owned(),
            automatic,
            ..Status::new(state, "c1")
        };
        for state in [State::Pending, State::Waiting, State::Queued] {
            assert!(status(state, true).lands_unasked("main", "c1"), "{state:?}");
            assert!(
                !status(state, false).lands_unasked("main", "c1"),
                "{state:?} on request"
            );
        }
        for state in [
            State::Off,
            State::Ready,
            State::Needs,
            State::NoExecutor,
            State::Returned,
        ] {
            assert!(
                !status(state, true).lands_unasked("main", "c1"),
                "{state:?}"
            );
        }
        let waiting = status(State::Waiting, true);
        assert!(!waiting.lands_unasked("other", "c1"), "another destination");
        assert!(!waiting.lands_unasked("main", "c2"), "another commit");
    }
}
