//! [`Manager::move_to`] (R1.06): an active workspace onto another change,
//! keeping its row, id, path, lease (expiry included: a move is not a
//! heartbeat), and backend.
//!
//! The steps, holding the repository's create lock shared (so GC and a
//! forget wait), the workspace's scan lock (so no snapshot or release runs
//! in between), and its automatic snapshots paused (so the files the move
//! writes are not snapshotted mid-move):
//!
//! 1. A final snapshot of the current change, of kind `move`, described
//!    `reef: move workspace <id> to <target>`. If it fails, nothing else
//!    happens: [`ManagerError::WriteInFlight`].
//! 2. (repository thread) Resolve the target as a create does, read its
//!    `reef.toml`, and hash its stores' lockfiles (R1.19, R1.20).
//! 3. Ensure the base for the target's tree, and plan the target's stores:
//!    one with no sealed build fails the move unless the request skips
//!    missing stores. Nothing has changed yet.
//! 4. (repository thread) Move the pointer: `edit` or `check_out`, one
//!    operation with the snapshot's description.
//! 5. (blocking) Unmount the stores, then `swap_base`: `NeedsCheckout`
//!    (clone, copy) checks the target's tree out in the workspace's own
//!    working copy, writing only the files that differ; `TreeReplaced`
//!    (overlay) resets it to the target, seeded with the base's
//!    `tree_state` as a create is. Then mount the target's stores, which is
//!    what makes a move to the same change remount a rebuilt store (ADR
//!    0022).
//! 6. Record the row (change, commit, base tree, attention) and take the
//!    target's snapshot spec.
//!
//! A failure in step 5 moves the pointer back in a second operation (which,
//! unlike jj's `edit`, never abandons a change the move edited) and mounts
//! the old stores again; both operations stay in the log and the row is
//! unchanged. A check-out that failed part way is undone by checking the old
//! tree out over the target's (the working copy reset to the target first,
//! so jj diffs from it); if that fails too, the log says the directory may
//! hold part of the target's files.

use std::sync::{Arc, PoisonError};
use std::time::Instant;

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::materialize::TREE_STATE_FILE;
use reef_lib::jj::{CheckoutSummary, CommitIdHex, PointerMove, StartPoint};
use reef_lib::telemetry::elapsed_ms;
use tracing::Instrument as _;

use super::create::resolve_start;
use super::stores::{StorePlan, StoreRules, StoreTarget};
use super::{Manager, ManagerError, MissingStores, Moved, Start, WorkspaceEntry};
use crate::backend::{BaseEntry, SwapOutcome};
use crate::model::{Attention, WorkspaceId, WorkspaceState};
use crate::repo::RepoHandle;
use crate::snapshot::{SnapshotKind, description};
use crate::store::MountedStore;

/// What step 5 did to the tree.
struct Swapped {
    tree_replaced: bool,
    checkout: CheckoutSummary,
}

/// Why step 5 failed, and whether the check-out had begun writing files.
struct SwapFailed {
    error: Box<ManagerError>,
    checkout_started: bool,
}

impl SwapFailed {
    fn before(error: impl Into<ManagerError>) -> Self {
        Self {
            error: Box::new(error.into()),
            checkout_started: false,
        }
    }
}

impl Manager {
    /// Moves an active workspace to `start` (a change to edit, or a new
    /// change on a revision); see the module docs for the steps.
    /// `missing_stores` is as for a create.
    ///
    /// Runs as its own task: a caller that stops waiting (an RPC whose
    /// client disconnected) must not cut a move between its pointer move
    /// and its check-out.
    #[tracing::instrument(skip_all, fields(workspace_id = %id))]
    pub async fn move_to(
        &self,
        id: WorkspaceId,
        start: Start,
        missing_stores: MissingStores,
    ) -> Result<Moved, ManagerError> {
        let manager = self
            .scheduler
            .manager()
            .ok_or(ManagerError::NotLoaded(id))?;
        let task = async move { manager.move_detached(id, start, missing_stores).await };
        tokio::spawn(task.instrument(tracing::Span::current())).await?
    }

    async fn move_detached(
        &self,
        id: WorkspaceId,
        start: Start,
        missing_stores: MissingStores,
    ) -> Result<Moved, ManagerError> {
        let begun = Instant::now();
        let entry = self.active_entry(id)?;
        let repo_id = entry.record().repo_id;
        let repo_lock = self.repo_lock(repo_id);
        let _shared = repo_lock.read().await;
        let _paused = self.scheduler.pause(id);
        let _scan = entry.scan_lock.lock().await;
        // A release may have finished while this waited.
        let record = entry.record();
        if record.state != WorkspaceState::Active {
            return Err(ManagerError::NotActive {
                id,
                repo: repo_id,
                state: record.state,
            });
        }
        let handle = self.handle(repo_id)?;
        let label = target_label(&start);
        let described = description(SnapshotKind::Move, id, Some(&label));

        // 1.
        let final_op_id = self.move_snapshot(&entry, described.clone()).await?;

        // 2 and 3.
        let resolved = resolve_start(&handle, start).await?;
        let (base, _) = self
            .ensure_base(&handle, &resolved, &*entry.backend)
            .await?;
        let rules = StoreRules {
            repo: repo_id,
            missing: missing_stores,
            skip: None,
        };
        let mounting = self.stores.mounting.read().await;
        let plan = self.plan_stores(&resolved.stores, &rules, &resolved.commit)?;

        // 4.
        let pointer = move_pointer(&handle, id, resolved.start.clone(), described).await?;

        // 5.
        let from = match self.base_cache(repo_id).get(&record.base_tree_id) {
            Ok(Some(from)) => from,
            // No backend reads `from`; it is there for one that would.
            Ok(None) | Err(_) => base.clone(),
        };
        let old_stores = entry.stores();
        let mut details = Vec::new();
        self.unmount_stores(&entry, &mut details).await;
        let swapped = match swap(self, &entry, &from, &base, &pointer).await {
            Ok(swapped) => swapped,
            Err(failed) => {
                let abandon_left = matches!(resolved.start, StartPoint::NewOn(_));
                self.roll_back(&entry, &handle, &pointer, abandon_left, old_stores)
                    .await;
                if failed.checkout_started {
                    restore_tree(&entry, &handle, &pointer).await;
                }
                return Err(*failed.error);
            }
        };
        let (mounted, stores_missing) = self.mount_target_stores(&entry, &plan).await;
        drop(mounting);

        // 6. Read again: the final snapshot and the watcher may have marked
        // it, and a recovering snapshot moves it to a new change.
        let before = entry.record();
        let same_change = before.change_id == pointer.wc.change_id.as_str();
        let attention = moved_attention(
            before.attention,
            stores_missing,
            same_change,
            swapped.tree_replaced,
        );
        let divergent = same_change && before.divergent;
        self.record_move(&entry, &pointer, &base, attention, divergent)?;
        let spec_changed = {
            let mut spec = entry.spec.lock().unwrap_or_else(PoisonError::into_inner);
            let changed = *spec != resolved.spec.snapshot;
            spec.clone_from(&resolved.spec.snapshot);
            changed
        };
        // A remounted overlay is a new mount to watch.
        if spec_changed || swapped.tree_replaced {
            self.scheduler.rewatch(id).await;
        }

        let record = entry.record();
        let lease = self.lease_of(&record)?;
        let op_id = pointer.op_id.as_ref().map(ToString::to_string);
        tracing::info!(
            repo = %repo_id,
            change_id = %record.change_id,
            commit_id = %record.commit_id,
            op_id = op_id.as_deref().unwrap_or(""),
            final_op_id = final_op_id.as_deref().unwrap_or(""),
            tree_replaced = swapped.tree_replaced,
            files_updated = swapped.checkout.updated,
            files_added = swapped.checkout.added,
            files_removed = swapped.checkout.removed,
            files_skipped = swapped.checkout.skipped,
            stores_mounted = mounted.len(),
            elapsed_ms = elapsed_ms(begun),
            "workspace moved"
        );
        Ok(Moved {
            record,
            lease,
            op_id,
            final_op_id,
            checkout: swapped.checkout,
            tree_replaced: swapped.tree_replaced,
            stores: mounted,
        })
    }

    /// The end of step 5: the target's planned stores mounted, and the
    /// entry's list replaced with them; whether one the target declares is
    /// not mounted (skipped as missing, or failed: the workspace has moved,
    /// so a failure is logged, not returned).
    async fn mount_target_stores(
        &self,
        entry: &WorkspaceEntry,
        plan: &StorePlan,
    ) -> (Vec<MountedStore>, bool) {
        let kind = entry.backend.kind();
        let mounted = match self
            .mount_stores(&plan.mount, &entry.paths.root, kind)
            .await
        {
            Ok(mounted) => mounted,
            Err(error) => {
                tracing::error!(workspace_id = %entry.paths.id, error = %ErrorChain(&error), "cannot mount the target's stores; the workspace moved without them");
                Vec::new()
            }
        };
        let missing = !plan.missing.is_empty() || mounted.len() < plan.mount.len();
        entry
            .stores
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone_from(&mounted);
        (mounted, missing)
    }

    /// Step 1: the move's final snapshot, and its operation if it recorded
    /// one. A snapshot that fails refuses the move.
    async fn move_snapshot(
        &self,
        entry: &Arc<WorkspaceEntry>,
        description: String,
    ) -> Result<Option<String>, ManagerError> {
        match self
            .snapshot_locked(entry, SnapshotKind::Move, description)
            .await
        {
            Ok(outcome) => Ok(outcome.changed.then(|| outcome.op_id.to_string())),
            Err(ManagerError::Snapshot(source)) => Err(ManagerError::WriteInFlight {
                id: entry.paths.id,
                source,
            }),
            Err(error) => Err(error),
        }
    }

    /// Step 6's row: the new change, commit, and base tree, `attention`,
    /// and `divergent`, in memory and then in the database. The loaded row
    /// follows the move even if the database write fails, since the files
    /// and the pointer have moved either way.
    fn record_move(
        &self,
        entry: &WorkspaceEntry,
        pointer: &PointerMove,
        base: &BaseEntry,
        attention: Option<Attention>,
        divergent: bool,
    ) -> Result<(), ManagerError> {
        let wc = &pointer.wc;
        entry.update_record(|record| {
            record.change_id = wc.change_id.to_string();
            record.commit_id = wc.commit_id.to_string();
            record.base_tree_id.clone_from(&base.tree_id);
            record.attention = attention;
            record.divergent = divergent;
        });
        self.db.move_workspace(
            entry.paths.id,
            wc.change_id.as_str(),
            wc.commit_id.as_str(),
            &base.tree_id,
            attention,
            divergent,
        )?;
        Ok(())
    }

    /// After a failed step 5: the pointer back where it was, in a second
    /// operation (abandoning the change it leaves only with `abandon_left`,
    /// the empty one a `NewOn` made), and the old stores mounted again. An
    /// overlay the swap left unmounted is marked `mount-failed`. Failures
    /// are logged.
    async fn roll_back(
        &self,
        entry: &Arc<WorkspaceEntry>,
        handle: &RepoHandle,
        pointer: &PointerMove,
        abandon_left: bool,
        old_stores: Vec<MountedStore>,
    ) {
        let id = entry.paths.id;
        if pointer.op_id.is_some() {
            let name = id.jj_workspace_name();
            let from = pointer.from.clone();
            let back = format!("reef: move workspace {id} to {from} (move failed)");
            let restored = handle
                .call_async(move |head| {
                    async move {
                        head.refresh().await?;
                        Ok::<_, ManagerError>(
                            head.repo
                                .restore_pointer(&name, &from, abandon_left, &back)
                                .await?,
                        )
                    }
                    .boxed_local()
                })
                .await;
            match restored {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot move the pointer back after a failed move");
                }
                Err(error) => {
                    tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot move the pointer back after a failed move");
                }
            }
        }
        let targets: Vec<StoreTarget> = old_stores
            .iter()
            .map(|store| StoreTarget {
                key: store.key.clone(),
                mount: store.mount.clone(),
                lockfile: store.lockfile.clone(),
                lockfile_id: store.lockfile_id.clone(),
            })
            .collect();
        let kind = entry.backend.kind();
        match self.mount_stores(&targets, &entry.paths.root, kind).await {
            Ok(mut mounted) => {
                // Mounted again, they are what they were.
                for (store, old) in mounted.iter_mut().zip(&old_stores) {
                    store.verified = old.verified;
                }
                *entry.stores.lock().unwrap_or_else(PoisonError::into_inner) = mounted;
            }
            Err(error) => {
                tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot mount the stores again after a failed move");
            }
        }
        let checked = {
            let entry = Arc::clone(entry);
            tokio::task::spawn_blocking(move || entry.backend.check_materialized(&entry.paths))
                .await
        };
        if let Ok(Err(error)) = checked {
            tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "the failed move left the workspace unmounted");
            match self
                .db
                .set_workspace_attention(id, Some(Attention::MountFailed), None)
            {
                Ok(()) => entry.set_attention(Some(Attention::MountFailed)),
                Err(error) => {
                    tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot mark the workspace");
                }
            }
        }
    }
}

/// How the move's descriptions name `start`: what the request named, since
/// the final snapshot is taken before it is resolved.
fn target_label(start: &Start) -> String {
    match start {
        Start::Change(text) => text.clone(),
        Start::NewOn(text) => format!("a new change on {text}"),
        Start::Default => "a new change".to_owned(),
    }
}

/// A moved workspace's attention. A move to the change it is on keeps a
/// mark about that change (divergent, stale-recovered), which says more.
/// Then `store-missing` when a target store is not mounted. Otherwise a
/// watch failure stays, and so do the overlay marks unless the backend
/// replaced the tree (a fresh upper, mounted); the rest, about the change
/// or the stores it left (or a final snapshot that has now succeeded),
/// clear.
fn moved_attention(
    current: Option<Attention>,
    stores_missing: bool,
    same_change: bool,
    tree_replaced: bool,
) -> Option<Attention> {
    if same_change && let Some(mark @ (Attention::Divergent | Attention::StaleRecovered)) = current
    {
        return Some(mark);
    }
    if stores_missing {
        return Some(Attention::StoreMissing);
    }
    match current {
        Some(Attention::WatchFailed) => Some(Attention::WatchFailed),
        Some(mark @ (Attention::UpperMissing | Attention::MountFailed)) if !tree_replaced => {
            Some(mark)
        }
        Some(
            Attention::Divergent
            | Attention::StaleRecovered
            | Attention::FinalSnapshotFailed
            | Attention::StoreHashMismatch
            | Attention::StoreMissing
            | Attention::UpperMissing
            | Attention::MountFailed,
        )
        | None => None,
    }
}

/// Step 4, on the repository thread.
async fn move_pointer(
    handle: &RepoHandle,
    id: WorkspaceId,
    start: StartPoint,
    description: String,
) -> Result<PointerMove, ManagerError> {
    let name = id.jj_workspace_name();
    handle
        .call_async(move |head| {
            async move {
                head.refresh().await?;
                Ok::<_, ManagerError>(head.repo.move_pointer(&name, start, &description).await?)
            }
            .boxed_local()
        })
        .await?
}

/// Step 5's tree, on the blocking pool with the jj handle locked: the
/// backend's swap, then the check-out or the seeded reset at the operation
/// that moved the pointer (the repository thread's head after it). The jj
/// handle is renewed first if another job of the repository panicked since
/// step 1's snapshot (#63). A failure says whether the check-out had begun.
async fn swap(
    manager: &Manager,
    entry: &Arc<WorkspaceEntry>,
    from: &BaseEntry,
    to: &BaseEntry,
    pointer: &PointerMove,
) -> Result<Swapped, SwapFailed> {
    let head = manager
        .workspace_head(entry, false)
        .await
        .map_err(SwapFailed::before)?;
    let entry = Arc::clone(entry);
    let (from, to) = (from.clone(), to.clone());
    let commit = pointer.wc.commit_id.clone();
    tokio::task::spawn_blocking(move || {
        let id = entry.paths.id;
        let jj = Arc::clone(&entry.jj);
        let mut guard = jj.lock().unwrap_or_else(PoisonError::into_inner);
        let ws = guard
            .as_mut()
            .ok_or_else(|| SwapFailed::before(ManagerError::NotLoaded(id)))?;
        let outcome = entry
            .backend
            .swap_base(&entry.paths, &from, &to)
            .map_err(SwapFailed::before)?;
        ws.set_head(head);
        match outcome {
            SwapOutcome::NeedsCheckout => {
                let checkout = pollster::block_on(ws.check_out_tree(&commit)).map_err(|error| {
                    SwapFailed {
                        error: Box::new(error.into()),
                        checkout_started: true,
                    }
                })?;
                if checkout.skipped > 0 {
                    tracing::warn!(
                        workspace_id = %id,
                        files_skipped = checkout.skipped,
                        "untracked files were in the way of the target's; they were kept and the target's not written"
                    );
                }
                Ok(Swapped {
                    tree_replaced: false,
                    checkout,
                })
            }
            SwapOutcome::TreeReplaced => {
                seed(ws, &to, &entry.paths.state_dir, &commit);
                Ok(Swapped {
                    tree_replaced: true,
                    checkout: CheckoutSummary::default(),
                })
            }
        }
    })
    .await
    .map_err(SwapFailed::before)?
}

/// After a check-out that failed part way and the pointer moved back: the
/// working copy reset to the target (what the files partly are), then the
/// old tree checked out over it, so jj rewrites every file the two trees
/// differ in and removes the target's own. Untracked files in the way are
/// kept. Failures are logged: the directory may then hold part of the
/// target's files, which the next snapshot would record.
async fn restore_tree(entry: &Arc<WorkspaceEntry>, handle: &RepoHandle, pointer: &PointerMove) {
    let id = entry.paths.id;
    let restored = async {
        let head = handle.call(|head| head.readonly()).await?;
        let entry = Arc::clone(entry);
        let (target, from) = (pointer.wc.commit_id.clone(), pointer.from.clone());
        tokio::task::spawn_blocking(move || {
            let jj = Arc::clone(&entry.jj);
            let mut guard = jj.lock().unwrap_or_else(PoisonError::into_inner);
            let ws = guard.as_mut().ok_or(ManagerError::NotLoaded(id))?;
            ws.set_head(head);
            pollster::block_on(ws.reset_to(&target))?;
            Ok::<_, ManagerError>(pollster::block_on(ws.check_out_tree(&from))?)
        })
        .await?
    }
    .await;
    match restored {
        Ok(checkout) => tracing::info!(
            workspace_id = %id,
            files_updated = checkout.updated,
            files_added = checkout.added,
            files_removed = checkout.removed,
            "the failed check-out was undone"
        ),
        Err(error) => tracing::error!(
            workspace_id = %id,
            error = %ErrorChain(&error),
            "cannot undo the failed check-out; the directory may hold part of the target's files"
        ),
    }
}

/// After `TreeReplaced`: the working copy reset to `commit` at the loaded
/// operation, then its `tree_state` replaced by the base's, so the next
/// snapshot is a no-op scan. The files are the target's already, so a
/// failure is logged and not returned: without the reset the next snapshot
/// finds the working copy stale with files that match its commit and
/// adopts it (ADR 0018); without the copy it rehashes every file.
fn seed(
    ws: &mut reef_lib::jj::Repo,
    base: &BaseEntry,
    state_dir: &std::path::Path,
    commit: &CommitIdHex,
) {
    if let Err(error) = pollster::block_on(ws.reset_to(commit)) {
        tracing::warn!(error = %ErrorChain(&error), "cannot reset the working copy after the swap; the next snapshot adopts the commit");
        return;
    }
    let from = base.tree_state_file();
    if let Err(error) = std::fs::copy(&from, state_dir.join(TREE_STATE_FILE)) {
        tracing::warn!(
            from = %from.display(),
            %error,
            "cannot seed the working copy from the base; the next snapshot rehashes every file"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_name_what_the_request_named() {
        assert_eq!(target_label(&Start::Change("kxyz".into())), "kxyz");
        assert_eq!(
            target_label(&Start::NewOn("main".into())),
            "a new change on main"
        );
        assert_eq!(target_label(&Start::Default), "a new change");
    }

    #[test]
    fn a_move_clears_marks_about_the_change_it_left() {
        let moved = |current, missing| moved_attention(current, missing, false, false);
        assert_eq!(moved(Some(Attention::Divergent), false), None);
        assert_eq!(moved(Some(Attention::StoreHashMismatch), false), None);
        assert_eq!(
            moved(Some(Attention::WatchFailed), false),
            Some(Attention::WatchFailed)
        );
        assert_eq!(
            moved(Some(Attention::WatchFailed), true),
            Some(Attention::StoreMissing)
        );
        assert_eq!(moved(None, false), None);
    }

    #[test]
    fn a_move_to_the_same_change_keeps_its_marks() {
        assert_eq!(
            moved_attention(Some(Attention::Divergent), true, true, false),
            Some(Attention::Divergent)
        );
        assert_eq!(
            moved_attention(Some(Attention::StoreHashMismatch), false, true, false),
            None,
            "the remount fixed it"
        );
    }

    #[test]
    fn a_replaced_tree_clears_the_overlay_marks() {
        assert_eq!(
            moved_attention(Some(Attention::UpperMissing), false, false, true),
            None
        );
        assert_eq!(
            moved_attention(Some(Attention::UpperMissing), false, false, false),
            Some(Attention::UpperMissing)
        );
    }
}
