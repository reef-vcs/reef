//! Carrying proposals across a rewrite of the changes below them (R3.11,
//! ADR 0084).
//!
//! jj rebases a change's descendants whenever it rewrites the change: an
//! amend, a describe, a landing (R3.16 rewrites every summarized one). A
//! proposal above it then reads `amended`, though its own diff is the one
//! proposed. A proposal is **carried** when its change's one visible commit
//! is the recorded commit rebased onto new commits of the same parent
//! changes, with no conflict and the same tree and description
//! ([`carry_check`](reef_lib::jj::lifecycle::carry_check)): the daemon
//! records a `proposed → proposed` transition at the new commit, cause
//! `dependency`, naming the parent change that moved and whether it was
//! amended or landed. A rebase the change's own agent asked for moves it
//! onto other parent changes, and is never carried (ADR 0071).
//!
//! One task, woken by every new operation, looks at each repository whose
//! head moved since it last looked; `LandChange` and stack transitions call
//! `carry` first for the changes they read, so a clean dependent never
//! answers `PROPOSAL_AMENDED`. The rule is a function of the recorded and
//! the visible commit, so a look after a crash does the same thing again,
//! and one after the carry finds nothing to do.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use reef_lib::error::ErrorChain;
use reef_lib::jj::lifecycle::{
    DependencyEvent, LifecycleState, NotCarried, TransitionCause, Verdicts as Known, carries,
};
use reef_lib::jj::{ChangeIdHex, CommitIdHex, OpKind, OpMeta};

use super::{Actor, LifecycleError, Planned, record};
use crate::daemon::AppState;
use crate::db::{LifecycleFilter, TransitionRow};
use crate::model::{RepoId, Timestamp};

/// How long the task waits after a wake-up, so a burst of operations is
/// looked at once.
const SETTLE: Duration = Duration::from_millis(200);

/// Proposals that moved checked in one look of a repository (S-247); the
/// rest wait for the next.
const MAX_CHECKS: usize = 64;

/// The states whose recorded commit a carry moves.
const CARRIED: [&str; 3] = ["proposed", "declined", "withdrawn"];

/// The agent the daemon's carries record.
pub const AGENT: &str = "reef-lifecycle";

/// Starts the task; it stops at shutdown.
pub(crate) fn start(state: &Arc<AppState>) {
    let weak = Arc::downgrade(state);
    let shutdown = state.shutdown.clone();
    let mut wakes = state.events.changes();
    tokio::spawn(async move {
        let mut seen: HashMap<RepoId, String> = HashMap::new();
        loop {
            if let Some(state) = weak.upgrade() {
                look(&state, &mut seen).await;
            } else {
                return;
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = wakes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(SETTLE) => {}
            }
        }
    });
}

/// Looks at every open repository whose head moved; `seen` holds the head
/// operation each was last looked at.
async fn look(state: &Arc<AppState>, seen: &mut HashMap<RepoId, String>) {
    for repo in state.repos.ids() {
        let Some(tip) = state.events.tip(repo) else {
            continue;
        };
        let op = reef_lib::jj::oplog::entry(tip.operation()).id.to_string();
        if seen.get(&repo) == Some(&op) {
            continue;
        }
        match carry(state, repo, None).await {
            // More than a look's share moved: look again next time.
            Ok(Carry { more: true, .. }) => {
                seen.remove(&repo);
            }
            Ok(_) => {
                seen.insert(repo, op);
            }
            Err(error) => {
                tracing::warn!(%repo, error = %ErrorChain(&error), "cannot carry proposals across their dependencies' rewrites");
            }
        }
        // Each proposal held by a change below it that left review is told,
        // once per change and text (decision 5), outside every lock.
        for (change, (missing, _)) in super::dependents::held(state, repo, None).await {
            super::dependents::tell_missing(state, repo, &change, &missing).await;
        }
    }
}

/// A superseded change and its successor.
pub(crate) type Renamed = Vec<(ChangeIdHex, ChangeIdHex)>;

/// Each repository's carry checks that did not carry, known while its
/// superseded changes stay the same: the carry follower and the
/// missing-dependency reads check each pair of a recorded and a visible
/// commit once (S-247), so the checks that can still carry are never
/// starved, and a collected commit is logged once.
#[derive(Debug, Default)]
pub(crate) struct Verdicts(Mutex<HashMap<RepoId, (Renamed, Known)>>);

impl Verdicts {
    /// `repo`'s known verdicts under `renamed`; none when `renamed`
    /// changed since they were reached.
    pub(crate) fn known(&self, repo: RepoId, renamed: &Renamed) -> Known {
        let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = all.entry(repo).or_default();
        if &entry.0 != renamed {
            *entry = (renamed.clone(), Known::new());
        }
        entry.1.clone()
    }

    /// Adds `found`, reached under `renamed`, logging each collected
    /// recorded commit once. With `current`, every pair a full look saw,
    /// forgets the others.
    pub(crate) fn remember(
        &self,
        repo: RepoId,
        renamed: &Renamed,
        found: Vec<(ChangeIdHex, (CommitIdHex, CommitIdHex), NotCarried)>,
        current: Option<&HashSet<(CommitIdHex, CommitIdHex)>>,
    ) {
        let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let entry = all.entry(repo).or_default();
        if &entry.0 != renamed {
            return;
        }
        if let Some(current) = current {
            entry.1.retain(|pair, _| current.contains(pair));
        }
        for (change, pair, not) in found {
            if not == NotCarried::Gone && !entry.1.contains_key(&pair) {
                tracing::warn!(%repo, change_id = %change, commit_id = %pair.0, "a proposal's recorded commit was collected; it is not carried, and does not hold the changes above it");
            }
            entry.1.insert(pair, not);
        }
    }
}

/// `repo`'s superseded changes with their successors.
pub(crate) fn renamed(state: &AppState, repo: RepoId) -> Result<Renamed, LifecycleError> {
    let superseded = state.db.lifecycle_page(
        repo,
        &LifecycleFilter {
            states: vec![LifecycleState::Superseded.as_str().to_owned()],
            changes: Vec::new(),
            agents: Vec::new(),
        },
        None,
        1000,
    )?;
    Ok(superseded
        .iter()
        .filter_map(|row| {
            Some((
                row.change_id.parse().ok()?,
                row.superseded_by.as_ref()?.parse().ok()?,
            ))
        })
        .collect())
}

/// What [`carry`] did.
#[derive(Debug, Default)]
pub(crate) struct Carry {
    /// The transitions recorded.
    pub rows: Vec<TransitionRow>,
    /// It stopped at [`MAX_CHECKS`].
    pub more: bool,
}

/// Who holds the repository's create lock, shared, and its lifecycle lock
/// while a carry is recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Locks {
    /// Nobody yet: the carry takes both.
    Free,
    /// The caller, since the operation that moved the proposals: no other
    /// carry can record them first. The owners of the ones left conflicted
    /// are told by the carry follower, outside the locks.
    Held,
}

/// Carries every proposal of `repo` (of `only`, when given: full change
/// ids, with no limit on the checks) whose change moved onto new commits of
/// the same parent changes with its own diff unchanged, in one operation.
/// Takes the repository's create lock shared and its lifecycle lock; the
/// caller holds neither.
pub(crate) async fn carry(
    state: &Arc<AppState>,
    repo: RepoId,
    only: Option<&[String]>,
) -> Result<Carry, LifecycleError> {
    carry_because(state, repo, only, None, Locks::Free).await
}

/// [`carry`], each carry naming `because`, the change below and what
/// happened, when given: a supersede names its successor for every
/// dependent it moved, its children's children too.
pub(crate) async fn carry_because(
    state: &Arc<AppState>,
    repo: RepoId,
    only: Option<&[String]>,
    because: Option<(&str, DependencyEvent)>,
    locks: Locks,
) -> Result<Carry, LifecycleError> {
    if only.is_some_and(<[String]>::is_empty) {
        return Ok(Carry::default());
    }
    let filter = LifecycleFilter {
        // A declined or withdrawn change follows too: a rebase by a
        // rewrite below it is not its owner's revision, and it stays out
        // of review (R3.11, S-245).
        states: [
            LifecycleState::Proposed,
            LifecycleState::Declined,
            LifecycleState::Withdrawn,
        ]
        .iter()
        .map(|state| state.as_str().to_owned())
        .collect(),
        changes: only.map(<[String]>::to_vec).unwrap_or_default(),
        agents: Vec::new(),
    };
    let rows = state.db.lifecycle_page(repo, &filter, None, 1000)?;
    if rows.is_empty() {
        return Ok(Carry::default());
    }
    let Some(tip) = state.events.tip(repo) else {
        return Ok(Carry::default());
    };
    let proposals: Vec<(ChangeIdHex, CommitIdHex)> = rows
        .iter()
        .filter(|row| only.is_none_or(|only| only.contains(&row.change_id)))
        .filter_map(|row| Some((row.change_id.parse().ok()?, row.commit_id.parse().ok()?)))
        .collect();
    // A superseded parent's dependents moved onto its successor (R3.11).
    let renamed = renamed(state, repo)?;
    let known = state.lifecycle.verdicts.known(repo, &renamed);
    let handle = state.manager.handle(repo)?;
    let max = if only.is_some() {
        usize::MAX
    } else {
        MAX_CHECKS
    };
    let found = {
        let renamed = renamed.clone();
        state
            .reads
            .run(&handle, move || async move {
                carries(&tip, &proposals, &renamed, &known, max).await
            })
            .await
            .map_err(|error| LifecycleError::Read(ErrorChain(&error).to_string()))?
            .map_err(|error| LifecycleError::Read(ErrorChain(&error).to_string()))?
    };
    let more = found.more;
    let current: Option<HashSet<_>> =
        (only.is_none() && !more).then(|| found.moved.iter().cloned().collect());
    state
        .lifecycle
        .verdicts
        .remember(repo, &renamed, found.verdicts, current.as_ref());
    // A rewrite below them left these conflicted: their owners are told
    // (R3.11, decision 5), once per change, commit and text.
    for conflicted in &found.conflicted {
        let proposed = rows.iter().any(|row| {
            row.change_id == conflicted.change.as_str()
                && row.state == LifecycleState::Proposed.as_str()
        });
        if let (true, Some(below), Locks::Free) = (proposed, conflicted.moved.first(), locks) {
            super::dependents::tell_conflicted(
                state,
                repo,
                conflicted.change.as_str(),
                below.as_str(),
            )
            .await;
        }
    }
    let found = found.carried;
    if found.is_empty() {
        return Ok(Carry {
            rows: Vec::new(),
            more,
        });
    }
    let create_lock = state.manager.repo_lock(repo);
    let lifecycle_lock = state.lifecycle.lock(repo);
    let _taken = match locks {
        Locks::Free => Some((create_lock.read_owned().await, lifecycle_lock.lock().await)),
        Locks::Held => None,
    };
    record_carries(state, repo, (&rows, because), &found, more).await
}

/// Records the carries of `found`, read from `rows`, each checked again
/// under the lifecycle lock, which the caller holds with the repository's
/// create lock, in one operation.
async fn record_carries(
    state: &Arc<AppState>,
    repo: RepoId,
    (rows, because): (&[crate::db::LifecycleRow], Option<(&str, DependencyEvent)>),
    found: &[reef_lib::jj::lifecycle::Carried],
    more: bool,
) -> Result<Carry, LifecycleError> {
    // What each carry names: the parent change that moved, and what
    // happened to it.
    let mut causes: HashMap<&str, (String, DependencyEvent)> = HashMap::new();
    for carried in found {
        let Some(moved) = carried.moved.first() else {
            continue;
        };
        let cause = if let Some((change, event)) = because {
            (change.to_owned(), event)
        } else {
            let landed = state
                .db
                .lifecycle_row(repo, moved.as_str())?
                .is_some_and(|row| row.state == LifecycleState::Landed.as_str());
            let event = if carried.renamed {
                DependencyEvent::Superseded
            } else if landed {
                DependencyEvent::Landed
            } else {
                DependencyEvent::Amended
            };
            (moved.to_string(), event)
        };
        causes.insert(carried.change.as_str(), cause);
    }
    // A supersede's children's children, carried in the same look, name
    // the successor as its children do, whichever carries them first.
    for _ in 0..found.len() {
        let mut inherited = false;
        for carried in found {
            let parent = carried
                .moved
                .first()
                .and_then(|moved| causes.get(moved.as_str()))
                .filter(|(_, event)| *event == DependencyEvent::Superseded)
                .cloned();
            if let Some(parent) = parent
                && causes.get(carried.change.as_str()) != Some(&parent)
            {
                causes.insert(carried.change.as_str(), parent);
                inherited = true;
            }
        }
        if !inherited {
            break;
        }
    }
    let mut planned = Vec::new();
    for carried in found {
        let Some(row) = state.db.lifecycle_row(repo, carried.change.as_str())? else {
            continue;
        };
        let recorded = rows
            .iter()
            .find(|seen| seen.change_id == row.change_id)
            .map(|seen| seen.commit_id.as_str());
        // Re-proposed, withdrawn or carried since the read: theirs stands.
        if !CARRIED.contains(&row.state.as_str()) || Some(row.commit_id.as_str()) != recorded {
            continue;
        }
        let Some((dependency, event)) = causes.get(carried.change.as_str()) else {
            continue;
        };
        planned.push(Planned {
            row: carry_row(repo, &row, carried.visible.as_str(), dependency, *event),
            owner: None,
        });
    }
    if planned.is_empty() {
        return Ok(Carry {
            rows: Vec::new(),
            more,
        });
    }
    let description = match planned.as_slice() {
        [one] => format!(
            "reef: carry change {} (change {} {})",
            one.row.change_id,
            one.row.dependency_change_id.as_deref().unwrap_or_default(),
            one.row.dependency_event.as_deref().unwrap_or_default()
        ),
        many => format!(
            "reef: carry {} proposals over the changes below them",
            many.len()
        ),
    };
    let meta = OpMeta::new(OpKind::Transition, description).agent(AGENT);
    let recorded = record(state, repo, planned, meta).await?;
    for row in &recorded.rows {
        tracing::info!(%repo, change_id = %row.change_id, commit_id = %row.commit_id, dependency = row.dependency_change_id.as_deref().unwrap_or_default(), "carried a proposal across its dependency's rewrite");
    }
    Ok(Carry {
        rows: recorded.rows,
        more,
    })
}

/// The pending row of a carry of `row`'s proposal to `commit`, because
/// `dependency` was rewritten (`event`).
pub(crate) fn carry_row(
    repo: RepoId,
    row: &crate::db::LifecycleRow,
    commit: &str,
    dependency: &str,
    event: DependencyEvent,
) -> TransitionRow {
    TransitionRow {
        repo_id: repo,
        id: crate::identity::new_id("trn_"),
        seq: None,
        committed: false,
        change_id: row.change_id.clone(),
        commit_id: commit.to_owned(),
        from_state: row.state.clone(),
        to_state: row.state.clone(),
        cause: TransitionCause::Dependency.as_str().to_owned(),
        round: row.round,
        reason: None,
        superseded_by: None,
        destination: row.destination.clone(),
        undoes_operation_id: None,
        undoes_transition_id: None,
        operation_id: None,
        operation_collected: false,
        agent: Some(AGENT.to_owned()),
        actor: Actor::Daemon.as_str().to_owned(),
        identity_id: None,
        identity_name: None,
        session_id: None,
        remote_token: None,
        remote_token_name: None,
        created_at: Timestamp::now(),
        recorded_at: None,
        dependency_change_id: Some(dependency.to_owned()),
        dependency_event: Some(event.as_str().to_owned()),
        web_session: None,
    }
}

/// Carries once, now, in `repo` (tests).
#[doc(hidden)]
pub async fn carry_now(state: &Weak<AppState>, repo: RepoId) -> Result<usize, String> {
    let Some(state) = state.upgrade() else {
        return Ok(0);
    };
    carry(&state, repo, None)
        .await
        .map(|carried| carried.rows.len())
        .map_err(|error| ErrorChain(&error).to_string())
}
