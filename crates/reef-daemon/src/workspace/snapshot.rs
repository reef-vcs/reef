//! The snapshot pipeline (R1.13): every snapshot of a workspace, explicit or
//! scheduled, runs [`Manager::run_snapshot`] with the workspace's scan lock
//! held.
//!
//! 1. (async) Take a scan permit; ask the repository thread for its head,
//!    reloaded first if an operation was written outside the daemon. Take
//!    the paths reported since the last scan, which a watcher snapshot of a
//!    notify workspace narrows its scan to (R1.30, see
//!    [`crate::snapshot`]).
//! 2. (blocking) Lock the jj handle, point it at that head, and
//!    `begin_snapshot`: lock the working copy, check freshness, scan. If the
//!    working copy is ahead of the head (the `jj` CLI ran on the registered
//!    root), reload the repository thread at head and try once more.
//! 3. (repository thread) If the scan changed anything, `commit_snapshot`;
//!    its result becomes the thread's head.
//! 4. (blocking) Update the row and index the operation as a local snapshot
//!    (R1.16), then save the working-copy state at the new operation (or at
//!    the head, when nothing was committed).
//! 5. (async) Log; when the working-copy commit moved, check the mounted
//!    stores' lockfiles against the commit (R1.20, on the repository
//!    thread), so the answer to an explicit snapshot already carries the
//!    mark; then run the post-snapshot hooks.
//!
//! Both rows are written before the working-copy state, as R1.16 requires
//! of its index row, so a crash between the two leaves rows naming an
//! operation that exists.

use std::path::{Path, PathBuf};
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
use crate::backend::BackendError;
use crate::db::Db;
use crate::model::{Attention, Timestamp, WorkspaceId, WorkspaceState};
use crate::snapshot::index::{self, SnapshotEntry};
use crate::snapshot::{ScanPermit, Scheduled, SnapshotKind, SnapshotOutcome, Trigger, description};

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
    /// Its working-copy lock and tree-state reload.
    load_ms: u64,
    /// Its walk of the files.
    walk_ms: u64,
    /// It covered only the reported paths.
    narrowed: bool,
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
        let record = entry.record();
        let state = record.state;
        let allowed =
            state == WorkspaceState::Active || (releasing_ok && state == WorkspaceState::Releasing);
        if !allowed {
            return Err(ManagerError::NotActive {
                id,
                repo: record.repo_id,
                state,
            });
        }
        self.snapshot_locked(&entry, kind, description).await
    }

    /// While the daemon shuts down, after the scheduler and the lease
    /// expiry loop have stopped: snapshots every workspace with watcher
    /// events not yet snapshotted (`kind = shutdown`), concurrently up to
    /// `watcher.max_concurrent_scans`. Leases are untouched; a restart gives
    /// them the restart grace. Failures are logged. Returns how many ran.
    pub async fn shutdown_snapshots(&self) -> usize {
        let dirty = self.scheduler.dirty();
        let kind = SnapshotKind::Shutdown;
        let snapshots = dirty.iter().map(|&id| async move {
            match self.snapshot_with(id, kind, description(kind, id, None)).await {
                Ok(_) => true,
                Err(error) => {
                    tracing::warn!(workspace_id = %id, error = %ErrorChain(&error), "shutdown snapshot failed");
                    false
                }
            }
        });
        let taken = futures::future::join_all(snapshots)
            .await
            .into_iter()
            .filter(|ok| *ok)
            .count();
        if !dirty.is_empty() {
            tracing::info!(
                dirty_count = dirty.len(),
                snapshot_count = taken,
                "shutdown snapshots taken"
            );
        }
        taken
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
    /// retries, the timer at its short interval after a failure.
    pub(crate) async fn scheduled_snapshot(&self, id: WorkspaceId, fallback: bool) -> Scheduled {
        let Some(entry) = self.entry(id) else {
            return Scheduled::Skipped;
        };
        let _scan = entry.scan_lock.lock().await;
        // A releasing workspace's final snapshot is the release's, and a
        // pause taken while this waited holds it off; the events stay
        // marked for after the pause.
        if entry.record().state != WorkspaceState::Active || self.scheduler.is_paused(id) {
            return Scheduled::Skipped;
        }
        let mut trigger = self.scheduler.take_trigger(id);
        trigger.from_events = !fallback;
        if trigger.first_event.is_none() && !fallback {
            // An explicit snapshot covered the events while this waited.
            return Scheduled::Skipped;
        }
        let kind = SnapshotKind::Watcher;
        let ran = self
            .run_snapshot(&entry, kind, description(kind, id, None), trigger)
            .await;
        match ran {
            Ok(_) => return Scheduled::Taken,
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
                // The workspace's edits since its last snapshot are not in
                // the store (a full disk, R1.23); the next trigger retries.
                tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "watcher snapshot failed");
            }
        }
        Scheduled::Failed
    }

    /// The pipeline; see the module docs.
    #[allow(
        clippy::too_many_lines,
        reason = "one snapshot's steps in order; the full-scan turn (R1.32) tipped it over"
    )]
    async fn run_snapshot(
        &self,
        entry: &Arc<WorkspaceEntry>,
        kind: SnapshotKind,
        description: String,
        mut trigger: Trigger,
    ) -> Result<SnapshotOutcome, ManagerError> {
        let begun = Instant::now();
        let started = trigger.first_event.unwrap_or(begun);
        let record = entry.record();
        let id = record.id;
        // The head first: a permit is a scan slot, not held while the
        // repository thread is busy.
        let head = self.workspace_head(entry, true).await?;
        let permit = self.scheduler.acquire().await;
        self.scheduler.scan_hold_point().await;
        self.scheduler.take_reported(id, &mut trigger);
        let full = trigger.prefixes().is_none();
        let permit = self
            .scheduler
            .full_scan_turn(permit, kind, full, record.watcher)
            .await;
        let ran = {
            let entry = Arc::clone(entry);
            let db = Arc::clone(&self.db);
            let expected = record.commit_id.parse::<CommitIdHex>().ok();
            let reefignore = entry.paths.root.join(REEFIGNORE);
            let spec = entry.spec();
            let prefixes = trigger.prefixes().map(<[PathBuf]>::to_vec);
            tokio::task::spawn_blocking(move || {
                refuse_symlinked_root(&entry.paths.root)?;
                // An overlay whose mount is gone would scan as empty.
                entry.backend.check_materialized(&entry.paths)?;
                let jj = Arc::clone(&entry.jj);
                let mut guard = jj.lock().unwrap_or_else(PoisonError::into_inner);
                let repo = guard.as_mut().ok_or(ManagerError::NotLoaded(id))?;
                let pipeline = Pipeline {
                    entry: &entry,
                    db: &db,
                    id,
                    kind,
                    expected: expected.as_ref(),
                    reefignore: &reefignore,
                    spec: &spec,
                    description: &description,
                    prefixes: prefixes.as_deref(),
                };
                pipeline.run(repo, head, permit)
            })
            .await??
        };
        self.scheduler.report_untracked(id, &ran.untracked);

        let (changed, adopted, divergent, recovered, op_id, commit_id, change_id) =
            match &ran.recorded {
                // The row, in the database and in memory, was written by the
                // blocking half before the working-copy state.
                Recorded::Committed(committed) => (
                    true,
                    false,
                    committed.divergent,
                    ran.recovered,
                    committed.op_id.clone(),
                    committed.commit_id.clone(),
                    committed.change_id.to_string(),
                ),
                Recorded::Unchanged {
                    head_op,
                    wc_commit,
                    wc_change,
                } => {
                    self.follow_view(entry, wc_commit, wc_change)?;
                    let (op, commit) = (head_op.clone(), wc_commit.clone());
                    (
                        false,
                        ran.adopted,
                        false,
                        false,
                        op,
                        commit,
                        wc_change.to_string(),
                    )
                }
            };
        let outcome = SnapshotOutcome {
            workspace: id,
            kind,
            changed,
            adopted,
            divergent,
            recovered,
            op_id,
            commit_id,
            change_id,
            latency_ms: elapsed_ms(started),
            paths_count: trigger.paths_count(),
            narrowed: ran.narrowed,
            scan_ms: ran.scan_ms,
            load_ms: ran.load_ms,
            walk_ms: ran.walk_ms,
        };
        log_outcome(
            &outcome,
            record.repo_id,
            ran.updated_files,
            trigger.everything,
            begun,
        );
        self.verify_stores(entry, &outcome, &record.commit_id).await;
        self.scheduler.run_hooks(&outcome);
        Ok(outcome)
    }
}

impl Manager {
    /// Points the row, in the database and in memory, at the view's
    /// working-copy commit after an unchanged snapshot, if it moved: the
    /// workspace adopted another's rewrite of the shared commit, or the view
    /// moved it (`jj abandon`).
    fn follow_view(
        &self,
        entry: &WorkspaceEntry,
        wc_commit: &CommitIdHex,
        wc_change: &ChangeIdHex,
    ) -> Result<(), ManagerError> {
        let record = entry.record();
        if wc_commit.as_str() == record.commit_id && wc_change.as_str() == record.change_id {
            return Ok(());
        }
        self.db
            .set_workspace_commit(record.id, wc_change.as_str(), wc_commit.as_str(), None)?;
        entry.update_record(|record| {
            record.commit_id = wc_commit.to_string();
            record.change_id = wc_change.to_string();
        });
        Ok(())
    }
}

/// The blocking half of a snapshot, with the jj handle locked.
struct Pipeline<'a> {
    entry: &'a WorkspaceEntry,
    db: &'a Db,
    id: WorkspaceId,
    kind: SnapshotKind,
    expected: Option<&'a CommitIdHex>,
    reefignore: &'a Path,
    spec: &'a SnapshotSpec,
    description: &'a str,
    /// What to narrow the scan to; `None` scans everything.
    prefixes: Option<&'a [PathBuf]>,
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
            prefixes: Option<&[PathBuf]>,
        ) -> Result<reef_lib::jj::PendingSnapshot<'r>, SnapshotError> {
            pollster::block_on(repo.begin_snapshot(expected, sources, prefixes))
        }
        let (expected, prefixes) = (self.expected, self.prefixes);
        match permit.pool() {
            Some(pool) => pool.install(move || scan(repo, expected, sources, prefixes)),
            None => scan(repo, expected, sources, prefixes),
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
        let millis =
            |time: std::time::Duration| u64::try_from(time.as_millis()).unwrap_or(u64::MAX);
        let scan_ms = millis(pending.scan_time());
        let load_ms = millis(pending.load_time());
        let walk_ms = millis(pending.walk_time());
        let narrowed = pending.narrowed();
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
                    load_ms,
                    walk_ms,
                    narrowed,
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
        let row = record_commit(self.db, self.entry, self.kind, &committed, recovered);
        let finished = pollster::block_on(pending.finish(&committed.op_id));
        let new_head = Some(Arc::clone(&committed.repo));
        let ran = row.and(finished.map_err(ManagerError::from)).map(|()| Ran {
            recorded: Recorded::Committed(committed),
            adopted: false,
            recovered,
            updated_files,
            untracked,
            scan_ms,
            load_ms,
            walk_ms,
            narrowed,
        });
        (ran, new_head)
    }
}

/// Refuses a root replaced by a symlink, which a scan would follow, taking
/// whatever it points at into the change (RX.01).
fn refuse_symlinked_root(root: &Path) -> Result<(), BackendError> {
    if std::fs::symlink_metadata(root).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
        return Err(BackendError::RootIsSymlink(root.to_owned()));
    }
    Ok(())
}

/// Writes a committed snapshot to the workspace's row, in the database and
/// in memory: commit, change, last snapshot, and the divergent or recovered
/// marks; then indexes the operation as a local snapshot of `kind` (R1.16).
/// The loaded row follows the operation even if the database write fails,
/// since the operation exists either way.
fn record_commit(
    db: &Db,
    entry: &WorkspaceEntry,
    kind: SnapshotKind,
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
    index::record(
        db,
        entry.record().repo_id,
        &SnapshotEntry {
            op_id: committed.op_id.clone(),
            workspace_id: id,
            commit_id: committed.commit_id.clone(),
            change_id: committed.change_id.clone(),
            kind,
            local: true,
            taken_at: Timestamp::from(committed.end_time),
        },
    )?;
    Ok(())
}

/// The snapshot's log event: `info` when it wrote an operation or adopted a
/// rewrite, `warn` when it diverged or recovered, `debug` otherwise.
/// `everything`: an event since the previous snapshot said anything may
/// have changed.
fn log_outcome(
    outcome: &SnapshotOutcome,
    repo: crate::model::RepoId,
    updated_files: u32,
    everything: bool,
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
            load_ms = outcome.load_ms,
            walk_ms = outcome.walk_ms,
            paths_count = outcome.paths_count,
            everything,
            narrowed = outcome.narrowed,
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
            everything,
            narrowed = outcome.narrowed,
            "{kind} snapshot found no changes"
        );
    }
}
