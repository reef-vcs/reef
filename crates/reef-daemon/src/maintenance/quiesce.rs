//! Quiesce and resume (R1.18, ADR 0020): hold the daemon still so a
//! filesystem snapshot of its state directory is consistent.
//!
//! A quiesce takes the maintenance lock (no GC or backup can start), holds
//! off every automatic snapshot (watcher events still mark workspaces
//! dirty) and every lease expiry tick, pauses the repository threads (every
//! one, or the named repository's) after the jobs already queued, so no jj
//! operation is written, refuses heartbeats, and then checkpoints SQLite's
//! write-ahead log into the database file (`TRUNCATE`). A request that
//! writes a row outside the repository threads (`Register`, the start of a
//! `Create`) can still add to the WAL afterwards, so a filesystem snapshot
//! must take the whole state directory, `-wal` and `-shm` with the
//! database. Heartbeats are refused (`Unavailable`) while
//! it lasts; Snapshot, Create, Release, Move, and Gc wait behind the
//! paused threads up to their own deadlines.
//!
//! It ends on [`resume`] with its token, or by itself `max` after it began
//! (at most `backup.quiesce_max`), so a caller that crashes cannot wedge
//! the daemon. On resume every active lease gets at least `lease.grace`
//! from now, so the pause cannot expire anyone. Shutdown resumes first.

use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use rand::RngExt as _;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use tokio::task::JoinHandle;

use super::{Busy, Guard, Task};
use crate::daemon::AppState;
use crate::db::DbError;
use crate::error::{Advice, Advise};
use crate::lease::TickPause;
use crate::model::{RepoId, Timestamp, duration_millis};
use crate::repo::{PauseGuard, RepoThreadError};
use crate::snapshot::AllPauseGuard;

/// Failure to quiesce or resume.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum QuiesceError {
    /// A quiesce is in effect already.
    #[error("the daemon is already quiesced, until {until}")]
    Active {
        /// When it resumes by itself.
        until: Timestamp,
    },
    /// Another maintenance task (GC, a backup) holds the lock.
    #[error(transparent)]
    Busy(#[from] Busy),
    /// No quiesce in effect has this token: wrong, or resumed already.
    #[error("no quiesce in effect has token {0:?}")]
    UnknownToken(String),
    /// The request is refused while the daemon is quiesced (`Heartbeat`).
    #[error("the daemon is quiesced for a backup until {until}")]
    Quiesced {
        /// When it resumes by itself.
        until: Timestamp,
    },
    /// The named repository is registered but not open, so its thread
    /// cannot be held.
    #[error("repository {0} is not open; it cannot be quiesced")]
    RepoNotOpen(RepoId),
    /// A repository thread could not be paused.
    #[error(transparent)]
    RepoThread(#[from] RepoThreadError),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
}

impl Classify for QuiesceError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Active { .. } | Self::Quiesced { .. } | Self::RepoNotOpen(_) => {
                ErrorKind::Unavailable
            }
            Self::Busy(error) => error.kind(),
            Self::UnknownToken(_) => ErrorKind::NotFound,
            Self::RepoThread(error) => error.kind(),
            Self::Db(error) => error.kind(),
        }
    }
}

impl Advise for QuiesceError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Quiesced { until } | Self::Active { until } => Some(Advice::hint(format!(
                "A backup holds the daemon still until {until} at the latest; retry then."
            ))),
            Self::Db(error) => error.advice(),
            _ => None,
        }
    }
}

/// What [`quiesce`] answers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Quiesced {
    /// Names the quiesce to [`resume`].
    pub token: String,
    /// When every pause was in place.
    pub quiesced_at: Timestamp,
    /// When it resumes by itself.
    pub auto_resume_at: Timestamp,
}

/// The quiesce in effect, if any; one per daemon, in
/// [`AppState::quiesce`].
#[derive(Debug, Default)]
pub struct Quiescer {
    active: Mutex<Option<Active>>,
}

/// Everything a quiesce holds; dropping it releases them.
#[derive(Debug)]
struct Active {
    token: String,
    auto_resume_at: Timestamp,
    begun: Instant,
    // Dropped in declaration order: the repository threads first, so the
    // jobs queued behind them run before snapshots and reclaims start.
    _repos: Vec<PauseGuard>,
    _snapshots: AllPauseGuard,
    _ticks: TickPause,
    _maintenance: Guard,
    timer: Option<JoinHandle<()>>,
}

impl Quiescer {
    /// When the quiesce in effect resumes by itself, if one is.
    pub fn until(&self) -> Option<Timestamp> {
        self.lock().as_ref().map(|active| active.auto_resume_at)
    }

    /// `Err(Quiesced)` while a quiesce is in effect: what `Heartbeat`
    /// answers then.
    pub fn check(&self) -> Result<(), QuiesceError> {
        match self.until() {
            Some(until) => Err(QuiesceError::Quiesced { until }),
            None => Ok(()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Active>> {
        self.active.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Quiesces the daemon, or only `repo`'s thread (snapshots, ticks, and GC
/// are held off for every repository either way), for at most `max`:
/// zero is `backup.quiesce_max`, and so is anything longer.
pub async fn quiesce(
    state: &Arc<AppState>,
    repo: Option<RepoId>,
    max: Duration,
) -> Result<Quiesced, QuiesceError> {
    let cap = state.config.file.backup.quiesce_max;
    let max = if max.is_zero() { cap } else { max.min(cap) };
    let maintenance = match state.maintenance.try_acquire(Task::Quiesce) {
        Ok(maintenance) => maintenance,
        Err(busy) => {
            return Err(match state.quiesce.until() {
                Some(until) => QuiesceError::Active { until },
                None => busy.into(),
            });
        }
    };
    let begun = Instant::now();
    let deadline = begun + max;
    let snapshots = state.manager.scheduler().pause_all();
    let ticks = state.manager.leases().pause_ticks();
    let ids = match repo {
        Some(id) => vec![id],
        None => state.repos.ids(),
    };
    let mut repos = Vec::with_capacity(ids.len());
    for id in ids {
        let Some(handle) = state.repos.get(id) else {
            // Named, it must be held still; among all, one that is not
            // open writes nothing through the daemon anyway.
            if repo.is_some() {
                return Err(QuiesceError::RepoNotOpen(id));
            }
            continue;
        };
        // What is left of `max`: the threads resume by themselves at the
        // same moment the timer below resumes the rest.
        let left = deadline.saturating_duration_since(Instant::now());
        repos.push(handle.pause(left).await?);
    }
    let token = format!("{:016x}", rand::rng().random::<u64>());
    let quiesced_at = Timestamp::now();
    let left = deadline.saturating_duration_since(Instant::now());
    let auto_resume_at = Timestamp(quiesced_at.0.saturating_add(duration_millis(left)));
    // In effect before the checkpoint, so no heartbeat writes after it; and
    // before the timer exists, so the timer always finds it.
    let mut slot = state.quiesce.lock();
    *slot = Some(Active {
        token: token.clone(),
        auto_resume_at,
        begun,
        _repos: repos,
        _snapshots: snapshots,
        _ticks: ticks,
        _maintenance: maintenance,
        timer: None,
    });
    let timer = {
        let state = Arc::downgrade(state);
        let token = token.clone();
        tokio::spawn(async move {
            tokio::time::sleep(left).await;
            auto_resume(&state, &token);
        })
    };
    if let Some(active) = slot.as_mut() {
        active.timer = Some(timer);
    }
    drop(slot);
    checkpoint(state, "TRUNCATE");
    if let Some(id) = repo {
        tracing::info!(%token, repo = %id, max_ms = duration_ms(max), "daemon quiesced");
    } else {
        tracing::info!(%token, max_ms = duration_ms(max), "daemon quiesced");
    }
    Ok(Quiesced {
        token,
        quiesced_at,
        auto_resume_at,
    })
}

/// Ends the quiesce `token` names, and returns how long it lasted.
pub fn resume(state: &AppState, token: &str) -> Result<Duration, QuiesceError> {
    let mut active = {
        let mut slot = state.quiesce.lock();
        match slot.as_ref() {
            Some(active) if active.token == token => slot.take(),
            _ => None,
        }
    }
    .ok_or_else(|| QuiesceError::UnknownToken(token.to_owned()))?;
    if let Some(timer) = active.timer.take() {
        timer.abort();
    }
    let paused = active.begun.elapsed();
    drop(active);
    extend_leases(state);
    tracing::info!(%token, paused_ms = duration_ms(paused), "daemon resumed");
    Ok(paused)
}

/// Resumes whatever quiesce is in effect; shutdown's first hook.
pub fn resume_any(state: &AppState) {
    let token = state
        .quiesce
        .lock()
        .as_ref()
        .map(|active| active.token.clone());
    if let Some(token) = token {
        tracing::info!(%token, "resuming the quiesce for shutdown");
        let _ = resume(state, &token);
    }
}

/// The timer's end: resumes `token` if it is still in effect.
fn auto_resume(state: &Weak<AppState>, token: &str) {
    let Some(state) = state.upgrade() else {
        return;
    };
    let mut slot = state.quiesce.lock();
    if slot.as_ref().is_none_or(|active| active.token != token) {
        return;
    }
    // This task is the timer; it must not abort itself before resuming.
    if let Some(active) = slot.as_mut() {
        active.timer = None;
    }
    drop(slot);
    tracing::warn!(%token, "quiesce reached its deadline without a resume; resuming");
    let _ = resume(&state, token);
}

/// After a resume: no lease may expire for the pause.
fn extend_leases(state: &AppState) {
    if let Err(error) = state.manager.leases().extend_after_pause() {
        tracing::error!(error = %ErrorChain(&error), "cannot extend the leases after a quiesce");
    }
}

/// `PRAGMA wal_checkpoint(<mode>)` on the daemon's connection. A failure
/// is logged: the database is consistent either way, only less compact.
pub(super) fn checkpoint(state: &AppState, mode: &str) {
    let sql = format!("PRAGMA wal_checkpoint({mode})");
    if let Err(error) = state.db.with(|conn| conn.execute_batch(&sql)) {
        tracing::warn!(error = %ErrorChain(&error), mode, "WAL checkpoint failed");
    }
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
