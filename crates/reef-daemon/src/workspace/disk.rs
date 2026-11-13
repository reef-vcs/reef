//! The manager's side of disk accounting (#272, ADR 0076; [`crate::disk`]):
//! walking a workspace, the background loop, the floor's check, and the
//! machine's status.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use reef_lib::error::ErrorChain;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::Manager;
use crate::backend::BackendKind;
use crate::disk::walk::{Caps, Layout, WalkInput, Walked};
use crate::disk::{DiskError, DiskFigure, Idle, Purgeable, Status, assess, deps};
use crate::model::{RepoId, Timestamp, WorkspaceId, WorkspaceRecord, WorkspaceState};

/// How often the loop looks for workspaces due a walk.
const TICK: Duration = Duration::from_secs(5);

/// One walk of a workspace (blocking): `dir` by `layout`, its files told
/// apart by the working copy's `tree_state` in `state_dir` and compared
/// with the base's in `base_state`, then #195's check under `root`.
fn walk_workspace(
    dir: std::path::PathBuf,
    layout: Layout,
    base_state: Option<std::path::PathBuf>,
    state_dir: &std::path::Path,
    skip: Vec<std::path::PathBuf>,
    root: &std::path::Path,
) -> std::io::Result<Walked> {
    let base = base_state.and_then(|dir| {
        reef_lib::jj::materialize::tree_state_entries(&dir)
            .map_err(|error| {
                tracing::debug!(error = %ErrorChain(&error), "cannot read the base's tree state");
            })
            .ok()
            .map(|entries| {
                entries
                    .into_iter()
                    .map(|entry| (entry.path, (entry.mtime_ms, entry.size)))
                    .collect::<HashMap<_, _>>()
            })
    });
    let tracked = reef_lib::jj::materialize::tree_state_entries(state_dir)
        .ok()
        .map(|entries| {
            entries
                .into_iter()
                .map(|entry| entry.path)
                .collect::<HashSet<_>>()
        });
    let input = WalkInput {
        dir,
        layout,
        base,
        tracked,
        skip,
        caps: Caps::DEFAULT,
    };
    let mut walked = crate::disk::walk::walk(&input)?;
    walked.figure.missing_dependencies = match (&input.tracked, &input.base) {
        (Some(tracked), _) => deps::missing(root, tracked),
        (None, Some(base)) => deps::missing(root, &base.keys().cloned().collect::<HashSet<_>>()),
        (None, None) => Vec::new(),
    };
    Ok(walked)
}

/// Where a walk runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Priority {
    /// On the blocking pool, now: a request is waiting.
    Request,
    /// On a thread of its own at lowered priority: the background loop.
    Background,
}

impl Manager {
    /// The disk state (#272).
    pub fn disk(&self) -> &Arc<crate::disk::Disk> {
        &self.disk
    }

    /// Walks the active workspace `id` now and keeps its figure. `None`
    /// (logged) when it is not loaded or not active, or the walk fails.
    pub(crate) async fn measure_disk(&self, id: WorkspaceId, priority: Priority) -> Option<Walked> {
        let entry = self.entry(id)?;
        let record = entry.record();
        // A releasing one too: its release reports what it frees.
        if !matches!(
            record.state,
            WorkspaceState::Active | WorkspaceState::Releasing
        ) {
            return None;
        }
        let base = match self.base_cache(record.repo_id).get(&record.base_tree_id) {
            Ok(base) => base,
            Err(error) => {
                tracing::warn!(workspace_id = %id, error = %ErrorChain(&error), "cannot read the base; tracked files count in full");
                None
            }
        };
        let paths = entry
            .paths
            .clone()
            .with_store_mounts(entry.stores().iter().map(|store| store.mount.as_str()));
        let (layout, dir) = match entry.backend.kind() {
            BackendKind::ApfsClone => (Layout::Apfs, paths.root.clone()),
            BackendKind::Reflink => (Layout::Reflink, paths.root.clone()),
            BackendKind::Copy => (Layout::Copy, paths.root.clone()),
            BackendKind::Overlayfs | BackendKind::FuseOverlayfs => {
                (Layout::OverlayUpper, paths.upper.clone())
            }
        };
        // One walk of a workspace at a time; a request's waits for a
        // background walk to end and walks again, so it covers what was
        // written before it asked.
        let walk_lock = self.disk.walk_lock(id);
        let _walking = walk_lock.lock().await;
        let _permit = match priority {
            Priority::Request => self.disk.request_permit().await,
            Priority::Background => None,
        };
        self.disk.walk_started(id);
        let root = paths.root.clone();
        let state_dir = paths.state_dir.clone();
        let skip = paths.store_mounts.clone();
        let base_state = base.map(|base| base.state_dir);
        let work = move || walk_workspace(dir, layout, base_state, &state_dir, skip, &root);
        let walked = match priority {
            Priority::Request => tokio::task::spawn_blocking(work)
                .await
                .map_err(|error| std::io::Error::other(error.to_string())),
            Priority::Background => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let spawned = std::thread::Builder::new()
                    .name("reef-disk-walk".to_owned())
                    .spawn(move || {
                        crate::symbols::lower_priority();
                        let _ = tx.send(work());
                    });
                match spawned {
                    Ok(_) => rx
                        .await
                        .map_err(|error| std::io::Error::other(error.to_string())),
                    Err(error) => Err(error),
                }
            }
        }
        .and_then(|walked| walked);
        let walked = match walked {
            Ok(walked) => walked,
            Err(error) => {
                tracing::warn!(workspace_id = %id, %error, "cannot measure the workspace's disk");
                return None;
            }
        };
        // Not if it ended meanwhile: its row is gone with it.
        if self
            .entry(id)
            .is_some_and(|entry| entry.record().state == WorkspaceState::Active)
        {
            if let Err(error) = self
                .db
                .put_workspace_disk(record.repo_id, id, &walked.figure)
            {
                tracing::warn!(workspace_id = %id, error = %ErrorChain(&error), "cannot keep the disk figure");
            }
            self.disk.walked(id);
        }
        Some(walked)
    }

    /// The kept figures of `repo`'s workspaces (or all), for the active
    /// `records`: a workspace never walked reads all zero, measured at its
    /// create.
    pub fn disk_figures<'a>(
        &self,
        repo: Option<RepoId>,
        records: impl IntoIterator<Item = &'a WorkspaceRecord>,
    ) -> HashMap<WorkspaceId, DiskFigure> {
        let mut kept = self.db.workspace_disks(repo).unwrap_or_else(|error| {
            tracing::warn!(error = %ErrorChain(&error), "cannot read the disk figures");
            HashMap::new()
        });
        records
            .into_iter()
            .filter(|record| record.state == WorkspaceState::Active)
            .map(|record| {
                let figure = kept.remove(&record.id).unwrap_or_else(|| DiskFigure {
                    measured_at: record.created_at,
                    ..DiskFigure::default()
                });
                (record.id, figure)
            })
            .collect()
    }

    /// The idle workspace whose release would free the most: active, with
    /// no live presence entry, and no snapshot (or create) for
    /// [`Disk::idle_after`](crate::disk::Disk::idle_after).
    pub(crate) fn largest_idle(&self) -> Option<Idle> {
        let busy: HashSet<String> = self
            .presence
            .entries()
            .into_iter()
            .map(|entry| entry.workspace_id)
            .filter(|id| !id.is_empty())
            .collect();
        let records = self.db.workspaces(None, &[WorkspaceState::Active]).ok()?;
        let figures = self.disk_figures(None, &records);
        let idle_before = Timestamp::now().0
            - i64::try_from(self.disk.idle_after().as_millis()).unwrap_or(i64::MAX);
        records
            .iter()
            .filter(|record| !busy.contains(&record.id.as_hex()))
            // A build writing output without snapshots is not idle.
            .filter(|record| !self.disk.written_within(record.id, self.disk.idle_after()))
            .filter_map(|record| {
                let since = record.last_snapshot_at.unwrap_or(record.created_at);
                (since.0 <= idle_before).then(|| Idle {
                    workspace: record.id,
                    repo: record.repo_id,
                    owner: record.owner.clone(),
                    bytes: figures.get(&record.id).map_or(0, DiskFigure::cost),
                    since,
                })
            })
            .max_by_key(|idle| (idle.bytes, std::cmp::Reverse(idle.since.0)))
    }

    /// Refuses a create or a store build when free space is under the
    /// floor (#272). Never called for snapshots, releases, moves or GC.
    /// `statvfs` only above the floor; on macOS one CoreFoundation call
    /// below it.
    pub(crate) fn check_disk_floor(&self) -> Result<(), DiskError> {
        self.disk.check_floor(|| self.largest_idle())
    }

    /// The machine's disk status. Blocking parts run off the async workers.
    pub async fn disk_status(self: &Arc<Self>, purgeable: Purgeable) -> Status {
        let manager = Arc::clone(self);
        tokio::task::spawn_blocking(move || manager.disk_status_blocking(purgeable))
            .await
            .unwrap_or_default()
    }

    fn disk_status_blocking(&self, purgeable: Purgeable) -> Status {
        let volumes = self.disk.volumes(purgeable);
        let records = self
            .db
            .workspaces(None, &[WorkspaceState::Active])
            .unwrap_or_default();
        let figures = self.disk_figures(None, &records);
        let workspaces = figures.values().map(DiskFigure::cost).sum();
        let stale = figures
            .iter()
            .filter(|(id, figure)| figure.incomplete || self.disk.is_dirty(**id))
            .count();
        let bases = self.db.base_cache_total_bytes().unwrap_or(0);
        let (stores, state_bytes) = self.disk.machine();
        let budget = volumes
            .iter()
            .find(|volume| volume.holds.contains(&"workspaces"))
            .map_or(0, |volume| self.disk.budget(volume.space.total));
        let mut status = Status {
            level: crate::disk::Level::Ok,
            reasons: Vec::new(),
            volumes,
            budget,
            workspaces,
            bases,
            stores,
            state: state_bytes,
            largest_idle: None,
            stale: u32::try_from(stale).unwrap_or(u32::MAX),
        };
        let (level, reasons) = assess(&status.volumes, status.reef(), budget);
        status.level = level;
        status.reasons = reasons;
        if level != crate::disk::Level::Ok {
            status.largest_idle = self.largest_idle();
        }
        status
    }

    /// Starts the background loop: every few seconds, walks the workspaces
    /// due one at a time; hourly, the machine figures and the Time Machine
    /// exclusions.
    pub fn start_disk_loop(self: &Arc<Self>, shutdown: CancellationToken) -> JoinHandle<()> {
        let manager = Arc::downgrade(self);
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut machine = tokio::time::interval(Duration::from_hours(1));
            machine.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    () = shutdown.cancelled() => break,
                    _ = machine.tick() => {
                        let Some(manager) = manager.upgrade() else { break };
                        let run = Arc::clone(&manager);
                        let _ = tokio::task::spawn_blocking(move || {
                            run.disk.walk_machine();
                            run.disk.exclude_from_backups(&run.db);
                        })
                        .await;
                    }
                    _ = tick.tick() => {
                        let Some(manager) = manager.upgrade() else { break };
                        manager.walk_due(&shutdown).await;
                    }
                }
            }
            tracing::debug!("disk loop stopped");
        })
    }

    /// Walks every workspace due now, one at a time.
    async fn walk_due(&self, shutdown: &CancellationToken) {
        let held: HashSet<String> = self
            .presence
            .entries()
            .into_iter()
            .map(|entry| entry.workspace_id)
            .collect();
        let due = self.disk.due(&|id| held.contains(&id.as_hex()));
        for id in due {
            if shutdown.is_cancelled() {
                return;
            }
            if self.measure_disk(id, Priority::Background).await.is_some() {
                self.disk.count_walk();
            } else {
                // Not loaded or failing: try again after an interval.
                self.disk.walked(id);
            }
        }
    }
}
