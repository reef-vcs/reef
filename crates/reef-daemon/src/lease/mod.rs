//! Leases (R1.15, ADR 0019): every workspace holds one, its holder renews
//! it with `Heartbeat`, and a lease that lapses past its grace has its
//! workspace reclaimed: a final snapshot recorded in the operation log, the
//! jj workspace forgotten, and the directory removed.
//!
//! The [`LeaseManager`] issues and renews leases, applies the restart grace
//! at daemon start, and runs the expiry loop. The reclaim pipeline itself
//! is [`Manager::reclaim`], shared with `Release` and (R1.18) restore.
//!
//! The lease id is the capability: nothing checks that a heartbeat comes
//! from the owner, who is informational (RX.01 reviews this).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::telemetry::elapsed_ms;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::Instrument as _;

use crate::config::LeaseSection;
use crate::db::{Db, DbError, Renewal};
use crate::error::{Advice, Advise};
use crate::model::{
    LeaseId, LeaseRecord, ReclaimReason, RepoId, Timestamp, WorkspaceId, WorkspaceRecord,
    WorkspaceState, duration_millis,
};
use crate::workspace::{
    Manager, ManagerError, ReclaimOutcome, ReleaseSpec, gone_advice, releasing_advice,
};

/// The shortest lease a request may ask for, so `heartbeat_every` (a third
/// of the length at most) stays a real interval.
pub const MIN_LENGTH: Duration = Duration::from_secs(1);

/// A failing reclaim is logged at `error` on its first tick and then once
/// every this many ticks; the ticks between log at `debug`.
const ERROR_EVERY_TICKS: u32 = 10;

/// Where a lease is in its life, computed from its row and the clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LeaseState {
    /// Before `expires_at`.
    Active,
    /// Past `expires_at`, within its grace: a heartbeat revives it.
    InGrace,
    /// Past its grace; the next tick reclaims its workspace.
    Expired,
    /// Released, expired and reclaimed, or replaced by a restore.
    Ended,
}

impl LeaseState {
    /// `lease`'s state at `now`.
    pub fn of(lease: &LeaseRecord, now: Timestamp) -> LeaseState {
        if lease.ended_at.is_some() {
            LeaseState::Ended
        } else if now <= lease.expires_at {
            LeaseState::Active
        } else if now <= grace_end(lease) {
            LeaseState::InGrace
        } else {
            LeaseState::Expired
        }
    }
}

/// `expires_at + grace`: the last moment a heartbeat revives the lease.
fn grace_end(lease: &LeaseRecord) -> Timestamp {
    Timestamp(
        lease
            .expires_at
            .0
            .saturating_add(duration_millis(lease.grace)),
    )
}

fn log_issued(lease: &LeaseRecord) {
    tracing::info!(
        workspace_id = %lease.workspace_id,
        lease_id = %lease.id,
        repo = %lease.repo_id,
        length_ms = duration_millis(lease.length),
        "lease issued"
    );
}

/// Failure to issue or renew a lease.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LeaseError {
    /// No workspace has this id.
    #[error("workspace {0} not found")]
    NotFound(WorkspaceId),
    /// The lease is not the workspace's current one.
    #[error("lease {lease} is not the current lease of workspace {workspace} (lease superseded)")]
    Superseded {
        /// The workspace.
        workspace: WorkspaceId,
        /// The lease the request named.
        lease: LeaseId,
    },
    /// The workspace was released or reclaimed.
    #[error("workspace {workspace} {} at {}", state.as_str(), at.map_or_else(|| "an unknown time".to_owned(), |at| at.to_string()))]
    Reclaimed {
        /// The workspace.
        workspace: WorkspaceId,
        /// Its repository, for the advice to create another.
        repo: RepoId,
        /// `released` or `reclaimed`.
        state: WorkspaceState,
        /// When its lease ended.
        at: Option<Timestamp>,
    },
    /// The workspace is being released or reclaimed now. Retryable: a
    /// release whose final snapshot fails goes back to `active`.
    #[error("workspace {0} is being released; retry")]
    Releasing(WorkspaceId),
    /// The lease is past its grace; its workspace is about to be reclaimed.
    #[error("lease {lease} of workspace {workspace} expired; its grace ended at {grace_ended_at}")]
    Expired {
        /// The workspace.
        workspace: WorkspaceId,
        /// Its repository, for the advice to create another.
        repo: RepoId,
        /// The lease.
        lease: LeaseId,
        /// `expires_at + grace`.
        grace_ended_at: Timestamp,
    },
    /// The requested lease is longer than `lease.max`.
    #[error("a lease of {requested:?} is longer than the maximum {max:?}")]
    TooLong {
        /// What was asked for.
        requested: Duration,
        /// `lease.max`.
        max: Duration,
    },
    /// The requested lease is shorter than [`MIN_LENGTH`].
    #[error("a lease of {0:?} is shorter than the minimum {MIN_LENGTH:?}")]
    TooShort(Duration),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
}

impl Classify for LeaseError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotFound(_) => ErrorKind::NotFound,
            Self::Superseded { .. } | Self::Reclaimed { .. } | Self::Expired { .. } => {
                ErrorKind::Conflict
            }
            Self::Releasing(_) => ErrorKind::Unavailable,
            Self::TooLong { .. } | Self::TooShort(_) => ErrorKind::InvalidArgument,
            Self::Db(error) => error.kind(),
        }
    }
}

impl Advise for LeaseError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Reclaimed {
                workspace,
                repo,
                state,
                ..
            } => Some(gone_advice(*workspace, *state, *repo)),
            Self::Expired {
                workspace, repo, ..
            } => Some(Advice::run(
                format!(
                    "The lease of workspace {workspace} ran out; it is being reclaimed. Create a new workspace."
                ),
                ["reef", "ws", "create", "--repo", &repo.as_hex()],
            )),
            Self::Releasing(_) => Some(releasing_advice()),
            Self::Db(error) => error.advice(),
            _ => None,
        }
    }
}

/// Issues and renews leases and reclaims the workspaces of expired ones.
/// One per daemon, owned by the [`Manager`] ([`Manager::leases`]).
pub struct LeaseManager {
    config: LeaseSection,
    db: Arc<Db>,
    manager: Weak<Manager>,
    /// Consecutive ticks on which each workspace's reclaim failed.
    failures: Mutex<HashMap<WorkspaceId, u32>>,
    /// Stops the expiry loop, between ticks and between reclaims.
    stop: CancellationToken,
}

impl std::fmt::Debug for LeaseManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LeaseManager")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl LeaseManager {
    /// A lease manager for the [`Manager`] that `manager` will point to.
    pub(crate) fn new(config: LeaseSection, db: Arc<Db>, manager: Weak<Manager>) -> LeaseManager {
        LeaseManager {
            config,
            db,
            manager,
            failures: Mutex::new(HashMap::new()),
            stop: CancellationToken::new(),
        }
    }

    /// The `[lease]` configuration.
    pub fn config(&self) -> &LeaseSection {
        &self.config
    }

    /// The lease length for a request: `lease.default` when none (or zero)
    /// is asked for; above `lease.max` is [`LeaseError::TooLong`], below
    /// [`MIN_LENGTH`] [`LeaseError::TooShort`].
    pub fn length_for(&self, requested: Option<Duration>) -> Result<Duration, LeaseError> {
        match requested {
            None => Ok(self.config.default),
            Some(length) if length.is_zero() => Ok(self.config.default),
            Some(length) if length > self.config.max => Err(LeaseError::TooLong {
                requested: length,
                max: self.config.max,
            }),
            Some(length) if length < MIN_LENGTH => Err(LeaseError::TooShort(length)),
            Some(length) => Ok(length),
        }
    }

    /// The lease length for a create: the request's, as
    /// [`LeaseManager::length_for`] checks it; when the request asks for
    /// none (or zero), the repository's `reef.toml` `[workspace] lease`
    /// (`from_spec`), capped at `lease.max` with a warning, since the
    /// repository's authors cannot know every daemon's maximum; else
    /// `lease.default`.
    pub fn length_with_spec(
        &self,
        requested: Option<Duration>,
        from_spec: Option<Duration>,
    ) -> Result<Duration, LeaseError> {
        let requested = requested.filter(|length| !length.is_zero());
        match (requested, from_spec) {
            (None, Some(length)) if length > self.config.max => {
                tracing::warn!(
                    requested_ms = u64::try_from(length.as_millis()).unwrap_or(u64::MAX),
                    max_ms = u64::try_from(self.config.max.as_millis()).unwrap_or(u64::MAX),
                    "reef.toml asks for a lease longer than lease.max; capped"
                );
                Ok(self.config.max)
            }
            // `Spec::parse` holds it to at least a minute, above MIN_LENGTH.
            (None, Some(length)) => Ok(length),
            _ => self.length_for(requested),
        }
    }

    /// How often a holder of a lease of `length` should heartbeat:
    /// `lease.heartbeat`, at most a third of `length`.
    pub fn heartbeat_every(&self, length: Duration) -> Duration {
        self.config.heartbeat.min(length / 3)
    }

    /// A new lease of `length` on `ws`, issued `now`, not yet written.
    pub fn new_lease(
        &self,
        ws: WorkspaceId,
        repo: RepoId,
        owner: &str,
        length: Duration,
        now: Timestamp,
    ) -> LeaseRecord {
        LeaseRecord {
            id: LeaseId::generate(),
            repo_id: repo,
            workspace_id: ws,
            owner: owner.to_owned(),
            issued_at: now,
            expires_at: Timestamp(now.0.saturating_add(duration_millis(length))),
            last_heartbeat_at: now,
            length,
            grace: self.config.grace,
            ended_at: None,
            end_reason: None,
        }
    }

    /// Writes a new workspace's row and its first lease in one transaction
    /// (`Create`'s last step).
    pub fn issue_with_workspace(
        &self,
        record: &WorkspaceRecord,
        lease: &LeaseRecord,
    ) -> Result<(), DbError> {
        self.db.insert_workspace_with_lease(record, lease)?;
        log_issued(lease);
        Ok(())
    }

    /// Issues a lease on an `active` workspace that has none (R1.18's
    /// restore re-leases this way; `Create` uses
    /// [`LeaseManager::issue_with_workspace`]).
    pub fn issue(
        &self,
        ws: WorkspaceId,
        repo: RepoId,
        owner: &str,
        length: Option<Duration>,
    ) -> Result<LeaseRecord, LeaseError> {
        let length = self.length_for(length)?;
        let lease = self.new_lease(ws, repo, owner, length, Timestamp::now());
        self.db.issue_lease(&lease)?;
        log_issued(&lease);
        Ok(lease)
    }

    /// Renews `lease` on `ws`: `expires_at = now + length`. Revives a lease
    /// within its grace. Never snapshots and never touches jj.
    pub fn heartbeat(&self, ws: WorkspaceId, lease: LeaseId) -> Result<LeaseRecord, LeaseError> {
        match self.db.renew_lease(ws, lease, Timestamp::now())? {
            Renewal::Renewed(record) => {
                tracing::debug!(
                    workspace_id = %ws,
                    lease_id = %lease,
                    repo = %record.repo_id,
                    expires_at = %record.expires_at,
                    "lease renewed"
                );
                Ok(record)
            }
            Renewal::NoWorkspace => Err(LeaseError::NotFound(ws)),
            Renewal::Ended { repo, state, at } => Err(LeaseError::Reclaimed {
                workspace: ws,
                repo,
                state,
                at,
            }),
            Renewal::Releasing => Err(LeaseError::Releasing(ws)),
            Renewal::Superseded => Err(LeaseError::Superseded {
                workspace: ws,
                lease,
            }),
            Renewal::PastGrace {
                repo,
                grace_ended_at,
            } => Err(LeaseError::Expired {
                workspace: ws,
                repo,
                lease,
                grace_ended_at,
            }),
        }
    }

    /// Gives every unended lease at least `lease.restart_grace` from now,
    /// so the first tick after a start reclaims nothing that was alive at
    /// shutdown. Returns how many leases moved.
    pub fn apply_restart_grace(&self) -> Result<usize, LeaseError> {
        let at_least = Timestamp(
            Timestamp::now()
                .0
                .saturating_add(duration_millis(self.config.restart_grace)),
        );
        let extended = self.db.extend_all_active_leases_to(at_least)?;
        let restart_grace_ms = duration_millis(self.config.restart_grace);
        if extended > 0 {
            tracing::info!(
                extended_count = extended,
                restart_grace_ms,
                "lease restart grace applied"
            );
        } else {
            tracing::debug!(restart_grace_ms, "no lease needed the restart grace");
        }
        Ok(extended)
    }

    /// Starts the expiry loop: every `lease.tick`, [`LeaseManager::tick_once`],
    /// the first one `lease.tick` from now.
    /// [`LeaseManager::stop`] ends it between ticks, or between two reclaims
    /// of one tick; a reclaim in progress always finishes.
    pub fn start_expiry_loop(self: Arc<Self>) -> JoinHandle<()> {
        tokio::spawn(async move {
            let first = tokio::time::Instant::now() + self.config.tick;
            let mut interval = tokio::time::interval_at(first, self.config.tick);
            interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    biased;
                    () = self.stop.cancelled() => break,
                    _ = interval.tick() => {}
                }
                if let Err(error) = self.tick_once().await {
                    tracing::error!(error = %ErrorChain(&error), "lease expiry tick failed");
                }
            }
            tracing::debug!("lease expiry loop stopped");
        })
    }

    /// Stops the expiry loop; await its handle to wait for a reclaim in
    /// progress.
    pub fn stop(&self) {
        self.stop.cancel();
    }

    /// One tick: reclaims, one at a time, every workspace whose lease is
    /// past its grace now. Returns the workspaces reclaimed. A reclaim that
    /// fails (its final snapshot, say) leaves the workspace `active` with
    /// its lease open, so the next tick retries; the error is logged on the
    /// first failing tick and every tenth after.
    pub async fn tick_once(&self) -> Result<Vec<WorkspaceId>, LeaseError> {
        let now = Timestamp::now();
        let expired = self.db.leases_expired_before(now)?;
        let Some(manager) = self.manager.upgrade() else {
            return Ok(Vec::new());
        };
        let mut reclaimed = Vec::new();
        let mut failed = HashSet::new();
        for lease in expired {
            if self.stop.is_cancelled() {
                break;
            }
            let ws = lease.workspace_id;
            match manager
                .reclaim(ws, ReclaimReason::Expired, ReleaseSpec::default())
                .await
            {
                Ok(_) => reclaimed.push(ws),
                // A heartbeat revived it, or a release got there first.
                Err(
                    error @ (ManagerError::LeaseNotExpired(_)
                    | ManagerError::NotActive {
                        state: WorkspaceState::Released | WorkspaceState::Reclaimed,
                        ..
                    }),
                ) => {
                    tracing::debug!(workspace_id = %ws, error = %ErrorChain(&error), "expired lease not reclaimed");
                }
                Err(error) => {
                    failed.insert(ws);
                    self.report_failure(&lease, &error);
                }
            }
        }
        self.failures
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|ws, _| failed.contains(ws));
        Ok(reclaimed)
    }

    /// Logs a failed reclaim, at `error` once every [`ERROR_EVERY_TICKS`]
    /// failing ticks of the workspace and at `debug` in between.
    fn report_failure(&self, lease: &LeaseRecord, error: &ManagerError) {
        let count = {
            let mut failures = self.failures.lock().unwrap_or_else(PoisonError::into_inner);
            let count = failures.entry(lease.workspace_id).or_insert(0);
            *count += 1;
            *count
        };
        let message = if matches!(error, ManagerError::FinalSnapshotFailed { .. }) {
            "final snapshot failed"
        } else {
            "cannot reclaim the workspace"
        };
        let chain = ErrorChain(error).to_string();
        if (count - 1) % ERROR_EVERY_TICKS == 0 {
            tracing::error!(
                workspace_id = %lease.workspace_id,
                lease_id = %lease.id,
                repo = %lease.repo_id,
                failed_ticks = count,
                error = %chain,
                "{message}; the workspace is kept and retried on the next tick"
            );
        } else {
            tracing::debug!(
                workspace_id = %lease.workspace_id,
                lease_id = %lease.id,
                failed_ticks = count,
                error = %chain,
                "{message}"
            );
        }
    }

    /// Reclaims `ws` for `reason` through the shared pipeline
    /// ([`Manager::reclaim`]); `Release` passes its own [`ReleaseSpec`].
    ///
    /// Runs as its own task, as snapshots do: a caller that stops waiting
    /// (a `Release` whose client disconnected, or one aborted at shutdown)
    /// must not cut the pipeline between forgetting the jj workspace and
    /// recording the reclaim.
    pub async fn reclaim(
        &self,
        ws: WorkspaceId,
        reason: ReclaimReason,
        spec: ReleaseSpec,
    ) -> Result<ReclaimOutcome, ManagerError> {
        let begun = Instant::now();
        let manager = self.manager.upgrade().ok_or(ManagerError::NotLoaded(ws))?;
        let task = async move { manager.reclaim(ws, reason, spec).await };
        let outcome = tokio::spawn(task.instrument(tracing::Span::current())).await?;
        tracing::debug!(workspace_id = %ws, elapsed_ms = elapsed_ms(begun), ok = outcome.is_ok(), "reclaim finished");
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lease(expires_at: i64, grace_ms: u64, ended_at: Option<i64>) -> LeaseRecord {
        LeaseRecord {
            id: "cccc000000000001".parse().unwrap(),
            repo_id: "aaaa000000000001".parse().unwrap(),
            workspace_id: "bbbb000000000001".parse().unwrap(),
            owner: "o".into(),
            issued_at: Timestamp(0),
            expires_at: Timestamp(expires_at),
            last_heartbeat_at: Timestamp(0),
            length: Duration::from_millis(1_000),
            grace: Duration::from_millis(grace_ms),
            ended_at: ended_at.map(Timestamp),
            end_reason: None,
        }
    }

    #[test]
    fn state_follows_expiry_and_grace() {
        let l = lease(1_000, 500, None);
        assert_eq!(LeaseState::of(&l, Timestamp(1_000)), LeaseState::Active);
        assert_eq!(LeaseState::of(&l, Timestamp(1_001)), LeaseState::InGrace);
        assert_eq!(LeaseState::of(&l, Timestamp(1_500)), LeaseState::InGrace);
        assert_eq!(LeaseState::of(&l, Timestamp(1_501)), LeaseState::Expired);
        let ended = lease(1_000, 500, Some(900));
        assert_eq!(LeaseState::of(&ended, Timestamp(0)), LeaseState::Ended);
    }

    fn manager(config: LeaseSection) -> LeaseManager {
        LeaseManager::new(config, Arc::new(Db::open_in_memory().unwrap()), Weak::new())
    }

    #[test]
    fn length_defaults_and_refuses_above_max() {
        let leases = manager(LeaseSection::default());
        assert_eq!(leases.length_for(None).unwrap(), Duration::from_mins(30));
        assert_eq!(
            leases.length_for(Some(Duration::ZERO)).unwrap(),
            Duration::from_mins(30)
        );
        assert_eq!(
            leases.length_for(Some(Duration::from_hours(24))).unwrap(),
            Duration::from_hours(24)
        );
        let error = leases
            .length_for(Some(Duration::from_hours(25)))
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        let error = leases
            .length_for(Some(Duration::from_millis(2)))
            .unwrap_err();
        assert!(matches!(error, LeaseError::TooShort(_)), "{error:?}");
    }

    #[test]
    fn heartbeat_every_is_at_most_a_third_of_the_length() {
        let leases = manager(LeaseSection::default());
        assert_eq!(
            leases.heartbeat_every(Duration::from_mins(30)),
            Duration::from_secs(60)
        );
        assert_eq!(
            leases.heartbeat_every(Duration::from_secs(90)),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn errors_classify() {
        let ws: WorkspaceId = "bbbb000000000001".parse().unwrap();
        let lease: LeaseId = "cccc000000000001".parse().unwrap();
        assert_eq!(
            LeaseError::Superseded {
                workspace: ws,
                lease
            }
            .kind(),
            ErrorKind::Conflict
        );
        let reclaimed = LeaseError::Reclaimed {
            workspace: ws,
            repo: "aaaa000000000001".parse().unwrap(),
            state: WorkspaceState::Reclaimed,
            at: Some(Timestamp(1_792_175_477_042)),
        };
        assert_eq!(reclaimed.kind(), ErrorKind::Conflict);
        assert_eq!(
            reclaimed.to_string(),
            "workspace bbbb000000000001 reclaimed at 2026-10-16T18:31:17.042Z"
        );
        assert_eq!(LeaseError::NotFound(ws).kind(), ErrorKind::NotFound);
    }
}
