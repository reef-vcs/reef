//! The snapshot pipeline (R1.13): every snapshot of a workspace, explicit or
//! scheduled, runs [`Manager::run_snapshot`] with the workspace's scan lock
//! held.
//!
//! 1. (async) Take a scan permit; ask the repository thread for its head,
//!    reloaded first if an operation was written outside the daemon.
//! 2. (blocking) Lock the jj handle, point it at that head, and
//!    `begin_snapshot`: lock the working copy, check freshness, scan. If the
//!    working copy is ahead of the head (the `jj` CLI ran on the registered
//!    root), reload the repository thread at head and try once more.
//! 3. (repository thread) If the scan changed anything, `commit_snapshot`;
//!    its result becomes the thread's head.
//! 4. (blocking) Update the row, then save the working-copy state at the new
//!    operation (or at the head, when nothing was committed).
//! 5. (async) Log, and run the post-snapshot hooks.
//!
//! The row is written before the working-copy state, as R1.16 requires of
//! its index row, so a crash between the two leaves a row naming an
//! operation that exists.

use std::path::Path;
use std::sync::{Arc, PoisonError};
use std::time::Instant;

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::snapshot::UntrackedReasonKind;
use reef_lib::jj::{
    ChangeIdHex, CommitIdHex, IgnoreSources, OperationIdHex, ReadonlyRepo, Repo, SnapshotCommit,
    SnapshotError, commit_snapshot,
};
use reef_lib::telemetry::elapsed_ms;
use tracing::Instrument as _;

use super::{Manager, ManagerError, SnapshotSpec, WorkspaceEntry};
use crate::db::Db;
use crate::model::{Attention, Timestamp, WorkspaceId, WorkspaceState};
use crate::snapshot::{ScanPermit, SnapshotKind, SnapshotOutcome, Trigger, description};

/// The per-workspace ignore file, in gitignore syntax, at the workspace root.
pub const REEFIGNORE: &str = ".reefignore";

/// What the blocking half of a snapshot did.
struct Ran {
    recorded: Recorded,
    adopted: bool,
    recovered: bool,
    updated_files: u32,
    untracked: Vec<(String, UntrackedReasonKind)>,
    /// jj's scan.
    scan_ms: u64,
}

/// Whether the snapshot wrote an operation.
enum Recorded {
    Committed(SnapshotCommit),
    Unchanged {
        /// The head the scan ran at.
        head_op: OperationIdHex,
        /// The view's working-copy commit there.
        wc_commit: CommitIdHex,
        /// Its change.
        wc_change: ChangeIdHex,
    },
}

impl Manager {
    /// Snapshots a workspace's files into its working-copy commit, after
    /// any snapshot in flight. `description` names the operation; empty
    /// for the default of `kind`. See [`crate::snapshot::Scheduler::snapshot_now`].
    #[tracing::instrument(skip_all, fields(workspace_id = %id, kind = kind.as_str()))]
    pub async fn snapshot(
        &self,
        id: WorkspaceId,
        kind: SnapshotKind,
        description: &str,
    ) -> Result<SnapshotOutcome, ManagerError> {
        let description = if description.is_empty() {
            self::description(kind, id, None)
        } else {
            description.to_owned()
        };
        self.snapshot_with(id, kind, description).await
    }

    /// [`Manager::snapshot`] with the description decided. A `Final` or
    /// `Shutdown` snapshot also runs on a `releasing` workspace.
    ///
    /// Runs as its own task: a caller that stops waiting (an RPC whose client
    /// disconnected) must not cut a snapshot between its commit and the row
    /// update and hooks that follow it.
    pub(crate) async fn snapshot_with(
        &self,
        id: WorkspaceId,
        kind: SnapshotKind,
        description: String,
    ) -> Result<SnapshotOutcome, ManagerError> {
        let manager = self
            .scheduler
            .manager()
            .ok_or(ManagerError::NotLoaded(id))?;
        let task = async move { manager.snapshot_detached(id, kind, description).await };
        tokio::spawn(task.instrument(tracing::Span::current())).await?
    }

    async fn snapshot_detached(
        &self,
        id: WorkspaceId,
        kind: SnapshotKind,
        description: String,
    ) -> Result<SnapshotOutcome, ManagerError> {
        let releasing_ok = matches!(kind, SnapshotKind::Final | SnapshotKind::Shutdown);
        let entry = self.loaded_entry(id, releasing_ok)?;
        let _scan = entry.scan_lock.lock().await;
        // A release may have finished while this waited.
        let state = entry.record().state;
        let allowed =
            state == WorkspaceState::Active || (releasing_ok && state == WorkspaceState::Releasing);
        if !allowed {
            return Err(ManagerError::NotActive { id, state });
        }
        self.snapshot_locked(&entry, kind, description).await
    }

    /// A snapshot with the entry's scan lock already held.
    pub(super) async fn snapshot_locked(
        &self,
        entry: &Arc<WorkspaceEntry>,
        kind: SnapshotKind,
        description: String,
    ) -> Result<SnapshotOutcome, ManagerError> {
        let id = entry.paths.id;
        let trigger = self.scheduler.take_trigger(id);
        let outcome = self.run_snapshot(entry, kind, description, trigger).await;
        self.scheduler.completed(id);
        outcome
    }

    /// A watcher (or, with `fallback`, a fallback timer) snapshot, from the
    /// scheduler. Failures are logged; the next event or the fallback timer
    /// retries. Returns whether a snapshot ran.
    pub(crate) async fn scheduled_snapshot(&self, id: WorkspaceId, fallback: bool) -> bool {
        let Some(entry) = self.entry(id) else {
            return false;
        };
        let _scan = entry.scan_lock.lock().await;
        // A releasing workspace's final snapshot is the release's, and a
        // pause taken while this waited holds it off; the events stay
        // marked for after the pause.
        if entry.record().state != WorkspaceState::Active || self.scheduler.is_paused(id) {
            return false;
        }
        let trigger = self.scheduler.take_trigger(id);
        if trigger.first_event.is_none() && !fallback {
            // An explicit snapshot covered the events while this waited.
            return false;
        }
        let kind = SnapshotKind::Watcher;
        let ran = self
            .run_snapshot(&entry, kind, description(kind, id, None), trigger)
            .await;
        match ran {
            Ok(_) => {}
            Err(ManagerError::NotLoaded(_)) => {
                // Its jj handle could not be reopened at start; logged then.
                tracing::debug!(workspace_id = %id, "watcher snapshot skipped: not loaded");
            }
            Err(ManagerError::Snapshot(error @ SnapshotError::RepoBehind { .. })) => {
                tracing::warn!(
                    workspace_id = %id,
                    error = %ErrorChain(&error),
                    "working copy is still ahead after reloading; retrying on the next trigger"
                );
            }
            Err(error) => {
                tracing::warn!(workspace_id = %id, error = %ErrorChain(&error), "watcher snapshot failed");
            }
        }
        true
    }

    /// The pipeline; see the module docs.
    async fn run_snapshot(
        &self,
        entry: &Arc<WorkspaceEntry>,
        kind: SnapshotKind,
        description: String,
        trigger: Trigger,
    ) -> Result<SnapshotOutcome, ManagerError> {
        let begun = Instant::now();
        let started = trigger.first_event.unwrap_or(begun);
        let record = entry.record();
        let id = record.id;
        // The head first: a permit is a scan slot, not held while the
        // repository thread is busy.
        let head = entry
            .repo
            .call_async(|head| head.refresh().boxed_local())
            .await??;
        let permit = self.scheduler.acquire().await;
        let ran = {
            let entry = Arc::clone(entry);
            let db = Arc::clone(&self.db);
            let expected = record.commit_id.parse::<CommitIdHex>().ok();
            let reefignore = entry.paths.root.join(REEFIGNORE);
            let spec = entry.spec();
            tokio::task::spawn_blocking(move || {
                let jj = Arc::clone(&entry.jj);
                let mut guard = jj.lock().unwrap_or_else(PoisonError::into_inner);
                let repo = guard.as_mut().ok_or(ManagerError::NotLoaded(id))?;
                let pipeline = Pipeline {
                    entry: &entry,
                    db: &db,
                    id,
                    expected: expected.as_ref(),
                    reefignore: &reefignore,
                    spec: &spec,
                    description: &description,
                };
                pipeline.run(repo, head, permit)
            })
            .await??
        };
        self.scheduler.report_untracked(id, &ran.untracked);

        let outcome = match &ran.recorded {
            Recorded::Committed(committed) => {
                // The row, in the database and in memory, was written by
                // the blocking half before the working-copy state.
                SnapshotOutcome {
                    workspace: id,
                    kind,
                    changed: true,
                    adopted: false,
                    divergent: committed.divergent,
                    recovered: ran.recovered,
                    op_id: committed.op_id.clone(),
                    commit_id: committed.commit_id.clone(),
                    change_id: committed.change_id.to_string(),
                    latency_ms: elapsed_ms(started),
                    paths_count: trigger.paths_count,
                    scan_ms: ran.scan_ms,
                }
            }
            Recorded::Unchanged {
                head_op,
                wc_commit,
                wc_change,
            } => {
                if wc_commit.as_str() != record.commit_id || wc_change.as_str() != record.change_id
                {
                    // Adopted another workspace's rewrite of the shared
                    // commit, or the view moved the workspace (`jj abandon`).
                    self.db.set_workspace_commit(
                        id,
                        wc_change.as_str(),
                        wc_commit.as_str(),
                        None,
                    )?;
                    entry.update_record(|record| {
                        record.commit_id = wc_commit.to_string();
                        record.change_id = wc_change.to_string();
                    });
                }
                SnapshotOutcome {
                    workspace: id,
                    kind,
                    changed: false,
                    adopted: ran.adopted,
                    divergent: false,
                    recovered: false,
                    op_id: head_op.clone(),
                    commit_id: wc_commit.clone(),
                    change_id: wc_change.to_string(),
                    latency_ms: elapsed_ms(started),
                    paths_count: trigger.paths_count,
                    scan_ms: ran.scan_ms,
                }
            }
        };
        log_outcome(&outcome, record.repo_id, ran.updated_files, begun);
        self.scheduler.run_hooks(&outcome);
        Ok(outcome)
    }
}

/// The blocking half of a snapshot, with the jj handle locked.
struct Pipeline<'a> {
    entry: &'a WorkspaceEntry,
    db: &'a Db,
    id: WorkspaceId,
    expected: Option<&'a CommitIdHex>,
    reefignore: &'a Path,
    spec: &'a SnapshotSpec,
    description: &'a str,
}

impl Pipeline<'_> {
    /// `begin_snapshot` on the scan slot's rayon pool, so jj-lib's directory
    /// jobs stay there.
    fn begin<'r>(
        &self,
        repo: &'r mut Repo,
        sources: &IgnoreSources<'_>,
        permit: &ScanPermit,
    ) -> Result<reef_lib::jj::PendingSnapshot<'r>, SnapshotError> {
        fn scan<'r>(
            repo: &'r mut Repo,
            expected: Option<&CommitIdHex>,
            sources: &IgnoreSources<'_>,
        ) -> Result<reef_lib::jj::PendingSnapshot<'r>, SnapshotError> {
            pollster::block_on(repo.begin_snapshot(expected, sources))
        }
        let expected = self.expected;
        match permit.pool() {
            Some(pool) => pool.install(move || scan(repo, expected, sources)),
            None => scan(repo, expected, sources),
        }
    }

    /// Steps 2 to 4. The scan permit is released once the scan is done.
    fn run(
        &self,
        repo: &mut Repo,
        head: Arc<ReadonlyRepo>,
        permit: ScanPermit,
    ) -> Result<Ran, ManagerError> {
        let sources = IgnoreSources {
            reefignore: Some(self.reefignore),
            excludes: &self.spec.excludes,
            mounts: &self.spec.mounts,
        };
        repo.set_head(head);
        let behind = match self.begin(repo, &sources, &permit) {
            Ok(pending) => {
                drop(permit);
                let (ran, new_head) = self.record(pending);
                if let Some(new_head) = new_head {
                    repo.set_head(new_head);
                }
                return ran;
            }
            Err(SnapshotError::RepoBehind { kind, wc_op, .. }) => (kind, wc_op),
            Err(error) => return Err(error.into()),
        };
        // The working copy moved past the head between `refresh` and the
        // scan (the `jj` CLI on this workspace): reload the repository
        // thread at head and try once more.
        tracing::info!(
            workspace_id = %self.id,
            op_id = %behind.1,
            "working copy is ahead of the daemon's head ({:?}); reloading",
            behind.0
        );
        let head = pollster::block_on(self.entry.repo.call_async(|head| {
            async move {
                head.reload().await?;
                Ok::<_, ManagerError>(head.readonly())
            }
            .boxed_local()
        }))??;
        repo.set_head(head);
        let pending = self.begin(repo, &sources, &permit)?;
        drop(permit);
        let (ran, new_head) = self.record(pending);
        if let Some(new_head) = new_head {
            repo.set_head(new_head);
        }
        ran
    }

    /// Steps 3 and 4: commit if anything changed, update the row, and save
    /// the working-copy state. Returns, beside the result, the head to load
    /// the jj handle at: the committed operation's, even when a later step
    /// failed, so the next snapshot finds the working copy stale rather than
    /// building a sibling of an operation that exists.
    fn record(
        &self,
        pending: reef_lib::jj::PendingSnapshot<'_>,
    ) -> (Result<Ran, ManagerError>, Option<Arc<ReadonlyRepo>>) {
        let adopted = pending.adopted();
        let updated_files = pending.updated_files();
        let scan_ms = u64::try_from(pending.scan_time().as_millis()).unwrap_or(u64::MAX);
        let untracked = pending.untracked().to_vec();
        if !pending.changed() {
            let head_op = pending.head_op().clone();
            let (Some(wc_commit), Some(wc_change)) =
                (pending.wc_commit().cloned(), pending.wc_change().cloned())
            else {
                // Unchanged implies the view had a working-copy commit.
                let error =
                    reef_lib::jj::repo::ReadError::NoWorkingCopyCommit(self.id.jj_workspace_name());
                return (Err(error.into()), None);
            };
            let ran = pollster::block_on(pending.finish(&head_op))
                .map(|()| Ran {
                    recorded: Recorded::Unchanged {
                        head_op,
                        wc_commit,
                        wc_change,
                    },
                    adopted,
                    recovered: false,
                    updated_files,
                    untracked,
                    scan_ms,
                })
                .map_err(ManagerError::from);
            return (ran, None);
        }

        let plan = pending.plan().clone();
        let recovered = plan.recover.is_some();
        let description = self.description.to_owned();
        let committed = pollster::block_on(self.entry.repo.call_async(move |head| {
            async move {
                let committed = commit_snapshot(head.repo.head(), &plan, &description).await;
                if let Ok(committed) = &committed {
                    // The next job starts from this operation (R1.03). A
                    // reload is not needed: nothing else in the daemon
                    // writes, and `refresh` catches anything outside it.
                    head.adopt(Arc::clone(&committed.repo));
                }
                committed
            }
            .boxed_local()
        }));
        let committed = match committed {
            Ok(Ok(committed)) => committed,
            Ok(Err(error)) => return (Err(error.into()), None),
            Err(error) => return (Err(error.into()), None),
        };
        let row = record_commit(self.db, self.entry, &committed, recovered);
        let finished = pollster::block_on(pending.finish(&committed.op_id));
        let new_head = Some(Arc::clone(&committed.repo));
        let ran = row.and(finished.map_err(ManagerError::from)).map(|()| Ran {
            recorded: Recorded::Committed(committed),
            adopted: false,
            recovered,
            updated_files,
            untracked,
            scan_ms,
        });
        (ran, new_head)
    }
}

/// Writes a committed snapshot to the workspace's row, in the database and
/// in memory: commit, change, last snapshot, and the divergent or recovered
/// marks. The loaded row follows the operation even if the database write
/// fails, since the operation exists either way.
fn record_commit(
    db: &Db,
    entry: &WorkspaceEntry,
    committed: &SnapshotCommit,
    recovered: bool,
) -> Result<(), ManagerError> {
    let now = Timestamp::now();
    entry.update_record(|record| {
        record.commit_id = committed.commit_id.to_string();
        record.change_id = committed.change_id.to_string();
        record.last_snapshot_op = Some(committed.op_id.to_string());
        record.last_snapshot_at = Some(now);
        if committed.divergent {
            record.divergent = true;
            record.attention = Some(Attention::Divergent);
        } else if recovered {
            record.divergent = false;
            record.attention = Some(Attention::StaleRecovered);
        }
    });
    let id = entry.paths.id;
    db.set_workspace_commit(
        id,
        committed.change_id.as_str(),
        committed.commit_id.as_str(),
        None,
    )?;
    db.record_snapshot(id, committed.op_id.as_str(), now)?;
    if committed.divergent {
        db.set_workspace_attention(id, Some(Attention::Divergent), Some(true))?;
    } else if recovered {
        db.set_workspace_attention(id, Some(Attention::StaleRecovered), Some(false))?;
    }
    Ok(())
}

/// The snapshot's log event: `info` when it wrote an operation or adopted a
/// rewrite, `warn` when it diverged or recovered, `debug` otherwise.
fn log_outcome(
    outcome: &SnapshotOutcome,
    repo: crate::model::RepoId,
    updated_files: u32,
    begun: Instant,
) {
    let kind = outcome.kind.as_str();
    if outcome.divergent {
        tracing::warn!(
            workspace_id = %outcome.workspace,
            %repo,
            op_id = %outcome.op_id,
            commit_id = %outcome.commit_id,
            change_id = %outcome.change_id,
            "another workspace rewrote this workspace's change while it had edits; the change is divergent"
        );
    } else if outcome.recovered {
        tracing::warn!(
            workspace_id = %outcome.workspace,
            %repo,
            op_id = %outcome.op_id,
            change_id = %outcome.change_id,
            "the workspace had no working-copy commit; recovered onto a new change"
        );
    }
    if outcome.changed {
        tracing::info!(
            workspace_id = %outcome.workspace,
            %repo,
            op_id = %outcome.op_id,
            commit_id = %outcome.commit_id,
            elapsed_ms = elapsed_ms(begun),
            latency_ms = outcome.latency_ms,
            scan_ms = outcome.scan_ms,
            paths_count = outcome.paths_count,
            "{kind} snapshot taken"
        );
    } else if outcome.adopted {
        tracing::info!(
            workspace_id = %outcome.workspace,
            %repo,
            commit_id = %outcome.commit_id,
            files_count = updated_files,
            elapsed_ms = elapsed_ms(begun),
            "working copy updated to its rewritten commit"
        );
    } else {
        tracing::debug!(
            workspace_id = %outcome.workspace,
            %repo,
            elapsed_ms = elapsed_ms(begun),
            paths_count = outcome.paths_count,
            "{kind} snapshot found no changes"
        );
    }
}
