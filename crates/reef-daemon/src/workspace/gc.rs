//! The jj step of GC (R1.17), with the repository's working copies held
//! still around it.
//!
//! [`reef_lib::jj::Repo::gc`] rewrites the operations it keeps, so they get
//! new ids, and a working copy last updated at an operation that is gone no
//! longer loads. Before it runs, every loaded workspace of the repository
//! has its automatic snapshots paused and its scan lock held (explicit
//! snapshots and releases wait), is snapshotted, and is checked: its
//! recorded tree must be its working-copy commit's tree at head. A stale
//! one is snapshotted again, which adopts the rewrite (no operation). The
//! moment the op heads move, inside the GC's job on the repository thread
//! and before the slow collection, each is moved to the new head
//! ([`reef_lib::jj::Repo::rebind_working_copy`]), the state R1.13's
//! freshness check calls fresh. If any workspace cannot be made to match,
//! or is not loaded, no operation is abandoned: it would be stranded.
//!
//! A crash between the two still strands working copies. Their files are
//! their commits', so the next daemon start (when it reopens them) and the
//! next GC (before it snapshots) move them to the head
//! ([`reef_lib::jj::Repo::rebind_if_stranded`]).
//!
//! The user's own jj workspaces (`default` at the root, any `jj workspace
//! add`) are moved by [`reef_lib::jj::Repo::gc`] itself, under jj's
//! working-copy lock; while a `jj` command holds one, nothing is abandoned
//! (ADR 0040).

use std::path::PathBuf;
use std::sync::{Arc, PoisonError};
use std::time::SystemTime;

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::{OperationIdHex, ReadonlyRepo};

use super::{Manager, ManagerError, WorkspaceEntry};
use crate::backend::fs_util::allocated_bytes;
use crate::model::{RepoId, WorkspaceId, WorkspaceState};
use crate::repo::RepoHandle;
use crate::snapshot::{SnapshotKind, description};

/// How many times a workspace that does not match its commit is
/// snapshotted again before GC gives up on abandoning operations. Each
/// retry only adopts another workspace's rewrite, so one is normally enough.
const MATCH_ROUNDS: usize = 3;

/// What the jj step did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JjStep {
    /// The operation store and git objects, before.
    pub bytes_before: u64,
    /// The same directories, after.
    pub bytes_after: u64,
    /// Rebuilding the index.
    pub reindex_ms: u64,
    /// Operations made unreachable.
    pub operations_abandoned: usize,
    /// Why the step stopped short, if it did; the other GC steps still run.
    pub error: Option<String>,
    /// The kept operations' old ids to their new ones
    /// ([`reef_lib::jj::GcOutcome::rewritten`]).
    pub rewritten: Vec<(OperationIdHex, OperationIdHex)>,
}

impl Manager {
    /// GC's jj step for `repo`; see the module docs. Failures are reported
    /// in [`JjStep::error`], not returned.
    pub async fn gc_jj(&self, repo: RepoId, handle: &RepoHandle, keep_newer: SystemTime) -> JjStep {
        let mut entries: Vec<Arc<WorkspaceEntry>> = self
            .read_entries()
            .values()
            .filter(|entry| entry.record().repo_id == repo)
            .cloned()
            .collect();
        entries.sort_by_key(|entry| entry.paths.id);
        let _paused: Vec<_> = entries
            .iter()
            .map(|entry| self.scheduler.pause(entry.paths.id))
            .collect();
        // In id order, as describe, abandon and undo take theirs (R2.03).
        let mut scans = Vec::with_capacity(entries.len());
        for entry in &entries {
            scans.push(entry.scan_lock.lock().await);
        }
        // A release that held a lock first has finished meanwhile.
        let active: Vec<&Arc<WorkspaceEntry>> = entries
            .iter()
            .filter(|entry| entry.record().state == WorkspaceState::Active)
            .collect();

        let unsafe_workspaces = self.settle(handle, &active).await;
        if !unsafe_workspaces.is_empty() {
            let bytes = match handle.call(|head| head.repo.gc_measured_dirs()).await {
                Ok(dirs) => measure(&dirs).await,
                Err(_) => 0,
            };
            return JjStep {
                bytes_before: bytes,
                bytes_after: bytes,
                error: Some(format!(
                    "no operations were abandoned: {}",
                    unsafe_workspaces.join("; ")
                )),
                ..JjStep::default()
            };
        }

        let wcs: Vec<Arc<WorkspaceEntry>> = active.iter().map(|entry| Arc::clone(entry)).collect();
        let ran = gc_job(handle, wcs, keep_newer).await;
        let (before, after, outcome, rebind_failures, readonly) = match ran {
            Ok(ran) => ran,
            Err(error) => {
                return JjStep {
                    error: Some(ErrorChain(&error).to_string()),
                    ..JjStep::default()
                };
            }
        };
        let mut step = JjStep {
            bytes_before: before,
            bytes_after: after,
            ..JjStep::default()
        };
        match outcome {
            Ok(outcome) => {
                step.reindex_ms = u64::try_from(outcome.reindex.as_millis()).unwrap_or(u64::MAX);
                step.operations_abandoned = outcome.operations_abandoned;
                step.rewritten = outcome.rewritten;
                if let Some(blocked) = outcome.abandon_blocked {
                    step.error = Some(format!(
                        "no operations were abandoned: {blocked}; run reef ws gc again when it finishes"
                    ));
                }
            }
            Err(error) => step.error = Some(ErrorChain(&error).to_string()),
        }
        if !rebind_failures.is_empty() {
            let failures = rebind_failures.join("; ");
            step.error = Some(match step.error {
                Some(error) => format!("{error}; {failures}"),
                None => failures,
            });
        }
        // Each handle follows the head; its next snapshot refreshes it anyway.
        for entry in active {
            let mut guard = entry.jj.lock().unwrap_or_else(PoisonError::into_inner);
            if let Some(repo) = guard.as_mut() {
                repo.set_head(Arc::clone(&readonly));
            }
        }
        step
    }

    /// Snapshots every workspace in `active`, then re-snapshots any whose
    /// recorded tree is not its commit's at head, up to [`MATCH_ROUNDS`]
    /// times. Returns why each workspace that still does not match (or
    /// cannot be checked) is unsafe to strand. The caller holds every scan
    /// lock.
    async fn settle(&self, handle: &RepoHandle, active: &[&Arc<WorkspaceEntry>]) -> Vec<String> {
        // A working copy an interrupted GC left on an abandoned operation
        // cannot snapshot; move it first if its files are its commit's.
        // A handle that cannot be renewed after a panicked job is on a
        // poisoned store, and GC's rebind would panic on it: unsafe.
        let mut unrenewed = Vec::new();
        for entry in active {
            match self.workspace_head(entry, true).await {
                Ok(head) => {
                    if let Err(error) = repair(entry, &head).await {
                        tracing::warn!(workspace_id = %entry.paths.id, error = %ErrorChain(&error), "cannot check the working copy before GC");
                    }
                }
                Err(error) => unrenewed.push(format!(
                    "workspace {}: {}",
                    entry.paths.id,
                    ErrorChain(&error)
                )),
            }
        }
        if !unrenewed.is_empty() {
            return unrenewed;
        }
        let mut pending: Vec<&Arc<WorkspaceEntry>> = active.to_vec();
        let mut problems = Vec::new();
        for round in 0..=MATCH_ROUNDS {
            for entry in &pending {
                let id = entry.paths.id;
                let kind = SnapshotKind::Explicit;
                if let Err(error) = self
                    .snapshot_locked(entry, kind, description(kind, id, None), None)
                    .await
                {
                    tracing::warn!(workspace_id = %id, error = %ErrorChain(&error), "snapshot before GC failed");
                }
            }
            let head = match handle.call_async(|head| head.refresh().boxed_local()).await {
                Ok(Ok(head)) => head,
                Ok(Err(error)) => {
                    return vec![format!("cannot load the head: {}", ErrorChain(&error))];
                }
                Err(error) => return vec![format!("cannot load the head: {}", ErrorChain(&error))],
            };
            problems.clear();
            let mut mismatched = Vec::new();
            for entry in active {
                match matches_head(entry, &head).await {
                    Ok(true) => {}
                    Ok(false) => {
                        mismatched.push(*entry);
                        problems.push(format!(
                            "workspace {} does not match its working-copy commit",
                            entry.paths.id
                        ));
                    }
                    Err(error) => problems.push(format!(
                        "workspace {}: {}",
                        entry.paths.id,
                        ErrorChain(&error)
                    )),
                }
            }
            if mismatched.is_empty() || round == MATCH_ROUNDS {
                break;
            }
            pending = mismatched;
        }
        problems
    }
}

/// Whether the workspace's recorded tree is its commit's in `head`.
async fn matches_head(
    entry: &Arc<WorkspaceEntry>,
    head: &Arc<ReadonlyRepo>,
) -> Result<bool, ManagerError> {
    let (entry, head, id) = (Arc::clone(entry), Arc::clone(head), entry.paths.id);
    tokio::task::spawn_blocking(move || {
        let mut guard = entry.jj.lock().unwrap_or_else(PoisonError::into_inner);
        let repo = guard.as_mut().ok_or(ManagerError::NotLoaded(id))?;
        repo.set_head(head);
        repo.working_copy_matches_view()
            .map_err(|error| ManagerError::WorkingCopyCheck { id, source: error })
    })
    .await?
}

/// What the GC's job on the repository thread returns: bytes before and
/// after, jj's outcome, the working copies that could not be moved, and the
/// head afterwards.
type JobResult = (
    u64,
    u64,
    Result<reef_lib::jj::GcOutcome, reef_lib::jj::GcError>,
    Vec<String>,
    Arc<ReadonlyRepo>,
);

/// Runs jj's GC as one job on the repository thread. The working copies in
/// `wcs` move to the new head inside it, the moment the op heads do and
/// before the slow collection, so a failure or crash after that leaves none
/// on an abandoned operation.
async fn gc_job(
    handle: &RepoHandle,
    wcs: Vec<Arc<WorkspaceEntry>>,
    keep_newer: SystemTime,
) -> Result<JobResult, crate::repo::RepoThreadError> {
    handle
        .call_async(move |head| {
            async move {
                let dirs = head.repo.gc_measured_dirs();
                let before = measure_blocking(&dirs);
                let mut rebind_failures = Vec::new();
                // Subscriptions read up to the rewritten head and move to
                // its rewrite (R2.09).
                let rewrite = head.begin_rewrite();
                let outcome = head
                    .repo
                    .gc_with(keep_newer, &mut |rewritten, op_id| {
                        if let Some(rewrite) = &rewrite {
                            rewrite.moved_to(rewritten, op_id);
                        }
                        for entry in &wcs {
                            if let Err(error) = rebind(entry, op_id) {
                                tracing::error!(
                                    workspace_id = %entry.paths.id,
                                    error = %ErrorChain(&error),
                                    "cannot move the workspace's working copy to the new head"
                                );
                                rebind_failures.push(format!(
                                    "workspace {}: {}",
                                    entry.paths.id,
                                    ErrorChain(&error)
                                ));
                            }
                        }
                    })
                    .await;
                drop(rewrite);
                let after = measure_blocking(&dirs);
                (before, after, outcome, rebind_failures, head.readonly())
            }
            .boxed_local()
        })
        .await
}

/// Moves the workspace's working copy to `op_id`, if it is not there
/// already. Runs on the repository thread, inside the GC's job.
fn rebind(entry: &WorkspaceEntry, op_id: &OperationIdHex) -> Result<(), ManagerError> {
    let id: WorkspaceId = entry.paths.id;
    let mut guard = entry.jj.lock().unwrap_or_else(PoisonError::into_inner);
    let repo = guard.as_mut().ok_or(ManagerError::NotLoaded(id))?;
    if repo.working_copy_op_id() != *op_id {
        pollster::block_on(repo.rebind_working_copy(op_id))
            .map_err(|error| ManagerError::Rebind { id, source: error })?;
    }
    Ok(())
}

/// Points the workspace's jj handle at `head` and repairs a working copy an
/// interrupted GC stranded ([`reef_lib::jj::Repo::rebind_if_stranded`]).
async fn repair(
    entry: &Arc<WorkspaceEntry>,
    head: &Arc<ReadonlyRepo>,
) -> Result<bool, ManagerError> {
    let (entry, head, id) = (Arc::clone(entry), Arc::clone(head), entry.paths.id);
    tokio::task::spawn_blocking(move || {
        let mut guard = entry.jj.lock().unwrap_or_else(PoisonError::into_inner);
        let repo = guard.as_mut().ok_or(ManagerError::NotLoaded(id))?;
        repo.set_head(head);
        pollster::block_on(repo.rebind_if_stranded())
            .map_err(|error| ManagerError::WorkingCopyCheck { id, source: error })
    })
    .await?
}

/// The bytes allocated under `dirs`, off the async workers.
pub(crate) async fn measure(dirs: &[PathBuf]) -> u64 {
    let dirs = dirs.to_vec();
    tokio::task::spawn_blocking(move || measure_blocking(&dirs))
        .await
        .unwrap_or(0)
}

/// The bytes allocated under `dirs`; a directory that cannot be walked
/// counts as empty, with a warning.
pub(crate) fn measure_blocking(dirs: &[PathBuf]) -> u64 {
    dirs.iter()
        .map(|dir| {
            allocated_bytes(dir).unwrap_or_else(|error| {
                tracing::warn!(path = %dir.display(), error = %ErrorChain(&error), "cannot measure a directory");
                0
            })
        })
        .sum()
}
