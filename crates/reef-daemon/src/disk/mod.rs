//! Disk accounting and the free-space floor (#272, ADR 0076;
//! `docs/disk.md`).
//!
//! - **One figure per active workspace**, from one capped walk
//!   ([`walk`]), kept in `workspace_disk`: tracked bytes, build output,
//!   unshared bytes, files, the largest untracked directories, and #195's
//!   missing dependencies. A watch event (ignored paths included) marks a
//!   workspace dirty; [`Manager`](crate::workspace::Manager)'s disk loop
//!   walks a dirty workspace at most every `[disk] measure_interval`, one
//!   background walk at a time machine-wide (requests' walks at most two,
//!   and never two of one workspace), and a held workspace whose figure is
//!   older than three intervals whether dirty or not (paths under
//!   `[snapshot] exclude`, and Watchman's subscription, send no events).
//!   `GetWorkspace` walks on request.
//! - **The floor**: free space under `[disk] floor` on the workspace
//!   root's or the state directory's volume refuses creates and store
//!   builds ([`DiskError::BelowFloor`], `DISK_SPACE_LOW`). Nothing else is
//!   refused. On macOS free space counts purgeable space, read only under
//!   twice the floor ([`volume`]).
//! - **The budget and the level**: `low` under the floor, `warn` under
//!   twice the floor or over the budget, else `ok`.
//! - **Time Machine**: [`backup`] marks Reef's caches excluded, once.

pub mod backup;
#[cfg(target_os = "macos")]
mod bulk;
pub mod deps;
pub mod volume;
pub mod walk;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use reef_lib::error::{Classify, ErrorKind};
use serde::{Deserialize, Serialize};

use crate::config::{Config, DiskSection};
use crate::model::{RepoId, Timestamp, WorkspaceId};

/// A workspace's disk figure, as of `measured_at`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskFigure {
    /// Allocated bytes of tracked files that differ from the base.
    pub tracked_bytes: u64,
    /// Allocated bytes of files the working copy does not track.
    pub build_bytes: u64,
    /// What releasing frees at once; `None` where it cannot be told.
    pub unshared_bytes: Option<u64>,
    /// Entries walked.
    pub files: u64,
    /// Up to three of the largest untracked directories, largest first.
    pub largest: Vec<DiskPathFigure>,
    /// #195: directories with a manifest and lockfile but nothing
    /// installed.
    pub missing_dependencies: Vec<String>,
    /// The walk stopped at a cap.
    pub incomplete: bool,
    /// When the walk ended, or the create before the first walk.
    pub measured_at: Timestamp,
}

impl Default for DiskFigure {
    fn default() -> Self {
        Self {
            tracked_bytes: 0,
            build_bytes: 0,
            unshared_bytes: None,
            files: 0,
            largest: Vec::new(),
            missing_dependencies: Vec::new(),
            incomplete: false,
            measured_at: Timestamp(0),
        }
    }
}

impl DiskFigure {
    /// What the workspace costs: its unshared bytes, else everything that
    /// differs from the base.
    pub fn cost(&self) -> u64 {
        self.unshared_bytes
            .unwrap_or(self.tracked_bytes + self.build_bytes)
    }
}

/// A directory and its bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskPathFigure {
    /// Repository-relative, `/`-separated.
    pub path: String,
    /// Allocated bytes under it.
    pub bytes: u64,
}

/// A disk level, worst last.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Nothing to say.
    #[default]
    Ok,
    /// Free space under twice the floor, or Reef over its budget.
    Warn,
    /// Free space under the floor.
    Low,
}

/// Why a level is not OK.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// Free space under the floor.
    BelowFloor,
    /// Free space under twice the floor.
    NearFloor,
    /// Reef holds more than its budget.
    OverBudget,
}

/// One volume Reef writes to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VolumeStatus {
    /// A directory on it.
    pub path: PathBuf,
    /// `workspaces`, `state`, or both.
    pub holds: Vec<&'static str>,
    /// Its size and `statvfs` free space.
    pub space: volume::Space,
    /// macOS's free space with purgeable space, when read.
    pub purgeable_free: Option<u64>,
    /// The floor here; 0 for none.
    pub floor: u64,
    /// Its level by free space alone.
    pub level: Level,
}

impl VolumeStatus {
    /// Free space as the floor counts it: the larger of `statvfs`'s and,
    /// when read, the purgeable figure.
    pub fn usable_free(&self) -> u64 {
        self.space.free.max(self.purgeable_free.unwrap_or_default())
    }
}

/// An active workspace nobody is using, and what releasing it frees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Idle {
    /// The workspace.
    pub workspace: WorkspaceId,
    /// Its repository.
    pub repo: RepoId,
    /// Its owner.
    pub owner: String,
    /// Its cost ([`DiskFigure::cost`]).
    pub bytes: u64,
    /// Its last snapshot, or its create.
    pub since: Timestamp,
}

/// The machine's disk figures.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// The worst of the reasons'.
    pub level: Level,
    /// Why it is not OK.
    pub reasons: Vec<Reason>,
    /// The volumes.
    pub volumes: Vec<VolumeStatus>,
    /// `[disk] budget` in bytes; 0 for none.
    pub budget: u64,
    /// Active workspaces' cost.
    pub workspaces: u64,
    /// The base cache's recorded bytes.
    pub bases: u64,
    /// Stores, as last walked.
    pub stores: u64,
    /// The state directory's own files, as last walked.
    pub state: u64,
    /// The idle workspace whose release frees the most.
    pub largest_idle: Option<Idle>,
    /// Active workspaces written to since their last walk, or whose last
    /// walk was incomplete.
    pub stale: u32,
}

impl Status {
    /// What Reef holds.
    pub fn reef(&self) -> u64 {
        self.workspaces + self.bases + self.stores + self.state
    }

    /// The volume nearest its floor, for one-line reports.
    pub fn tightest(&self) -> Option<&VolumeStatus> {
        self.volumes
            .iter()
            .min_by_key(|volume| i128::from(volume.usable_free()) - i128::from(volume.floor))
    }
}

/// A create or store build refused for disk.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DiskError {
    /// Free space is under the floor.
    #[error(
        "{} free on the volume holding {}, under Reef's floor of {}",
        bytes(*free),
        volume.display(),
        bytes(*floor)
    )]
    BelowFloor {
        /// A directory on the volume.
        volume: PathBuf,
        /// Usable free space.
        free: u64,
        /// The floor.
        floor: u64,
        /// The idle workspace whose release frees the most.
        idle: Option<Idle>,
    },
}

impl Classify for DiskError {
    fn kind(&self) -> ErrorKind {
        match self {
            // Retrying is useless until space is freed (ADR 0073).
            Self::BelowFloor { .. } => ErrorKind::Conflict,
        }
    }
}

impl DiskError {
    /// `DISK_SPACE_LOW` and its metadata.
    pub(crate) fn reason(&self) -> (&'static str, Vec<(&'static str, String)>) {
        match self {
            Self::BelowFloor {
                volume,
                free,
                floor,
                idle,
            } => {
                let mut pairs = vec![
                    ("volume", volume.display().to_string()),
                    ("free_bytes", free.to_string()),
                    ("floor_bytes", floor.to_string()),
                ];
                if let Some(idle) = idle {
                    pairs.extend([
                        ("largest_idle", idle.workspace.as_hex()),
                        ("largest_idle_owner", idle.owner.clone()),
                        ("largest_idle_bytes", idle.bytes.to_string()),
                    ]);
                }
                (reef_api::error::DISK_SPACE_LOW, pairs)
            }
        }
    }

    /// Release the largest idle workspace, else list them to choose.
    pub(crate) fn advice(&self) -> reef_lib::error::Advice {
        match self {
            Self::BelowFloor { idle, .. } => {
                let hint = "New workspaces and store builds are refused until space is freed. \
                     Releasing a workspace takes a final snapshot first, so nothing written is lost.";
                match idle {
                    Some(idle) => reef_lib::error::Advice::run(
                        format!(
                            "{hint} The largest idle one is {} ({}, {}).",
                            idle.workspace.as_hex(),
                            idle.owner,
                            bytes(idle.bytes)
                        ),
                        ["reef", "ws", "release", &idle.workspace.as_hex()],
                    ),
                    None => reef_lib::error::Advice::run(hint, ["reef", "ws", "list"]),
                }
            }
        }
    }
}

/// `bytes` as `du -h` would print it, in binary units.
pub fn bytes(bytes: u64) -> String {
    reef_client::json::human_bytes(bytes)
}

/// Whether to read macOS's purgeable free space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Purgeable {
    /// Only for a volume whose `statvfs` free space is under twice its
    /// floor, where it can change the level.
    NearFloor,
    /// For every volume (`GetDaemonStatus`).
    Always,
}

/// How long a volume's free space is reused.
const SPACE_TTL: Duration = Duration::from_secs(5);

/// A held workspace whose figure is older than this many intervals is
/// walked whether dirty or not.
const HELD_REFRESH_INTERVALS: u32 = 3;

/// Walks on a request's behalf (`GetWorkspace`) at once, machine-wide;
/// background walks run one at a time on the disk loop.
const REQUEST_WALKS: usize = 2;

/// A workspace counts as idle after this long without a snapshot or a
/// live presence entry.
pub const IDLE_AFTER: Duration = Duration::from_mins(30);

/// Per-workspace scheduling state.
#[derive(Clone, Copy, Debug, Default)]
struct Tracking {
    /// Written to since its last walk began.
    dirty: bool,
    /// When its last walk ended.
    walked: Option<Instant>,
    /// Its last watch event.
    written: Option<Instant>,
}

/// The machine figures walked hourly.
#[derive(Clone, Copy, Debug, Default)]
struct Machine {
    stores: u64,
    state: u64,
}

/// The daemon's disk state: scheduling, the volumes and the floor. One per
/// daemon, in the manager.
#[derive(Debug)]
pub struct Disk {
    config: DiskSection,
    /// `(directory, what it holds)`: the workspace root's and the state
    /// directory's.
    dirs: Vec<(PathBuf, &'static str)>,
    state_dir: PathBuf,
    stores_dir: PathBuf,
    repos_dir: PathBuf,
    /// The directories marked excluded from Time Machine when they exist.
    backup_dirs: Vec<PathBuf>,
    tracking: Mutex<HashMap<WorkspaceId, Tracking>>,
    machine: Mutex<Machine>,
    space_cache: Mutex<HashMap<PathBuf, (Instant, volume::Space)>>,
    /// macOS's purgeable free space by directory, reused for [`SPACE_TTL`]:
    /// each read is a CoreFoundation call of 9 to 43 ms.
    purgeable_cache: Mutex<HashMap<PathBuf, (Instant, Option<u64>)>>,
    /// Per workspace, serializing its walks ([`Disk::walk_lock`]).
    walk_locks: Mutex<HashMap<WorkspaceId, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// Bounds request walks ([`REQUEST_WALKS`]).
    request_walks: tokio::sync::Semaphore,
    /// Background walks done, for tests.
    walks: AtomicU64,
    hooks: Hooks,
}

/// Test hooks.
#[derive(Debug, Default)]
struct Hooks {
    /// Every volume's free space, as if `statvfs` said so.
    free: Mutex<Option<u64>>,
    /// `measure_interval`, below its configured minimum.
    interval: Mutex<Option<Duration>>,
    /// [`IDLE_AFTER`], shortened.
    idle_after: Mutex<Option<Duration>>,
    /// Purgeable reads made.
    purgeable_reads: AtomicU64,
}

impl Disk {
    /// The disk state for `config`.
    pub fn new(config: &Config) -> Disk {
        let workspaces = config.workspace_base.clone().unwrap_or_else(|| {
            // A configured root may name `<repo-short>`; its fixed prefix
            // is on the same volume.
            let root = PathBuf::from(&config.file.workspace.root);
            let fixed: PathBuf = root
                .components()
                .take_while(|part| !part.as_os_str().to_string_lossy().contains('<'))
                .collect();
            if fixed.is_absolute() {
                fixed
            } else {
                config.paths.state_dir.join(fixed)
            }
        });
        let stores_dir = config.paths.state_dir.join("stores");
        let mut backup_dirs = vec![stores_dir.clone()];
        if let Some(base) = &config.workspace_base {
            backup_dirs.insert(0, base.clone());
        }
        Disk {
            config: config.file.disk.clone(),
            dirs: vec![
                (workspaces, "workspaces"),
                (config.paths.state_dir.clone(), "state"),
            ],
            state_dir: config.paths.state_dir.clone(),
            stores_dir,
            repos_dir: config.paths.repos.clone(),
            backup_dirs,
            tracking: Mutex::new(HashMap::new()),
            machine: Mutex::new(Machine::default()),
            space_cache: Mutex::new(HashMap::new()),
            purgeable_cache: Mutex::new(HashMap::new()),
            walk_locks: Mutex::new(HashMap::new()),
            request_walks: tokio::sync::Semaphore::new(REQUEST_WALKS),
            walks: AtomicU64::new(0),
            hooks: Hooks::default(),
        }
    }

    /// `[disk]`.
    pub fn config(&self) -> &DiskSection {
        &self.config
    }

    /// The least time between two background walks of a dirty workspace.
    pub fn measure_interval(&self) -> Duration {
        lock(&self.hooks.interval).unwrap_or(self.config.measure_interval)
    }

    /// How long without a snapshot or a presence entry makes a workspace
    /// idle.
    pub fn idle_after(&self) -> Duration {
        lock(&self.hooks.idle_after).unwrap_or(IDLE_AFTER)
    }

    /// Marks `workspace` written to: the loop walks it once the interval
    /// since its last walk has passed. Cheap: called for every watch event.
    pub fn mark_dirty(&self, workspace: WorkspaceId) {
        // Only a tracked one: an event racing a release is dropped.
        if let Some(tracking) = lock(&self.tracking).get_mut(&workspace) {
            tracking.dirty = true;
            tracking.written = Some(Instant::now());
        }
    }

    /// Starts tracking `workspace` (at create and at load) with no walk
    /// yet: `dirty` for a loaded one, whose writes while the daemon was
    /// down are unknown.
    pub fn track(&self, workspace: WorkspaceId, dirty: bool) {
        lock(&self.tracking).insert(
            workspace,
            Tracking {
                dirty,
                walked: (!dirty).then(Instant::now),
                written: None,
            },
        );
    }

    /// Stops tracking `workspace`, which ended.
    pub fn forget(&self, workspace: WorkspaceId) {
        lock(&self.tracking).remove(&workspace);
        lock(&self.walk_locks).remove(&workspace);
    }

    /// A walk of `workspace` begins: what is written from now on marks it
    /// dirty again, so a write during the walk is never lost.
    pub fn walk_started(&self, workspace: WorkspaceId) {
        if let Some(tracking) = lock(&self.tracking).get_mut(&workspace) {
            tracking.dirty = false;
        }
    }

    /// Records a walk of `workspace` that ended now (or failed: the next
    /// try waits an interval).
    pub fn walked(&self, workspace: WorkspaceId) {
        if let Some(tracking) = lock(&self.tracking).get_mut(&workspace) {
            tracking.walked = Some(Instant::now());
        }
    }

    /// Whether `workspace` had a watch event within `within`: a build
    /// writing output without snapshots is not idle.
    pub fn written_within(&self, workspace: WorkspaceId, within: Duration) -> bool {
        lock(&self.tracking)
            .get(&workspace)
            .and_then(|tracking| tracking.written)
            .is_some_and(|at| at.elapsed() < within)
    }

    /// The lock that serializes walks of `workspace`: a background walk
    /// and a request's never run at once, and each answer covers writes
    /// made before it was asked.
    pub(crate) fn walk_lock(
        &self,
        workspace: WorkspaceId,
    ) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        std::sync::Arc::clone(lock(&self.walk_locks).entry(workspace).or_default())
    }

    /// At most [`REQUEST_WALKS`] request walks at once, machine-wide.
    pub(crate) async fn request_permit(&self) -> Option<tokio::sync::SemaphorePermit<'_>> {
        self.request_walks.acquire().await.ok()
    }

    /// Counts a background walk.
    pub(crate) fn count_walk(&self) {
        self.walks.fetch_add(1, Ordering::Relaxed);
    }

    /// Background walks done so far.
    pub fn walks(&self) -> u64 {
        self.walks.load(Ordering::Relaxed)
    }

    /// Whether `workspace` was written to since its last walk.
    pub fn is_dirty(&self, workspace: WorkspaceId) -> bool {
        lock(&self.tracking)
            .get(&workspace)
            .is_some_and(|tracking| tracking.dirty)
    }

    /// The workspaces due a walk now, oldest walk first: dirty ones whose
    /// last walk is at least the interval old (or never walked), and
    /// `held` ones whose last walk is three intervals old.
    pub fn due(&self, held: &dyn Fn(WorkspaceId) -> bool) -> Vec<WorkspaceId> {
        let interval = self.measure_interval();
        let now = Instant::now();
        let mut due: Vec<(Option<Instant>, WorkspaceId)> = lock(&self.tracking)
            .iter()
            .filter(|(id, tracking)| {
                let age = tracking.walked.map(|at| now.saturating_duration_since(at));
                let dirty_due = tracking.dirty && age.is_none_or(|age| age >= interval);
                let held_due =
                    age.is_some_and(|age| age >= interval * HELD_REFRESH_INTERVALS) && held(**id);
                dirty_due || held_due
            })
            .map(|(id, tracking)| (tracking.walked, *id))
            .collect();
        due.sort();
        due.into_iter().map(|(_, id)| id).collect()
    }

    /// The workspaces tracked and dirty now.
    pub fn dirty_count(&self) -> usize {
        lock(&self.tracking)
            .values()
            .filter(|tracking| tracking.dirty)
            .count()
    }

    /// Each volume Reef writes to, the workspace root's first, one entry
    /// per device. Blocking: call off the async workers.
    pub fn volumes(&self, purgeable: Purgeable) -> Vec<VolumeStatus> {
        let mut volumes: Vec<(u64, VolumeStatus)> = Vec::new();
        for (dir, holds) in &self.dirs {
            let at = volume::existing_ancestor(dir);
            let Ok(device) = volume::device(&at) else {
                continue;
            };
            if let Some((_, existing)) = volumes.iter_mut().find(|(dev, _)| *dev == device) {
                existing.holds.push(holds);
                continue;
            }
            let Some(space) = self.space(&at) else {
                continue;
            };
            let floor = self.config.floor.resolve(space.total, volume::auto_floor);
            volumes.push((
                device,
                VolumeStatus {
                    path: at,
                    holds: vec![holds],
                    space,
                    purgeable_free: None,
                    floor,
                    level: Level::Ok,
                },
            ));
        }
        volumes
            .into_iter()
            .map(|(_, mut status)| {
                let near = status.space.free < status.floor.saturating_mul(2);
                if purgeable == Purgeable::Always || near {
                    status.purgeable_free = self.purgeable(&status.path);
                }
                status.level = level_of(status.usable_free(), status.floor);
                status
            })
            .collect()
    }

    /// `statvfs` of `dir`, reused for [`SPACE_TTL`], or the test hook's.
    fn space(&self, dir: &Path) -> Option<volume::Space> {
        let forced = *lock(&self.hooks.free);
        if let Some(cached) = lock(&self.space_cache)
            .get(dir)
            .filter(|(at, _)| at.elapsed() < SPACE_TTL)
            .map(|(_, space)| *space)
        {
            return Some(with_forced(cached, forced));
        }
        match volume::space(dir) {
            Ok(space) => {
                lock(&self.space_cache).insert(dir.to_owned(), (Instant::now(), space));
                Some(with_forced(space, forced))
            }
            Err(error) => {
                tracing::warn!(dir = %dir.display(), %error, "cannot read free space");
                None
            }
        }
    }

    fn purgeable(&self, dir: &Path) -> Option<u64> {
        if let Some(forced) = *lock(&self.hooks.free) {
            self.hooks.purgeable_reads.fetch_add(1, Ordering::Relaxed);
            return Some(forced);
        }
        if let Some((_, cached)) = lock(&self.purgeable_cache)
            .get(dir)
            .filter(|(at, _)| at.elapsed() < SPACE_TTL)
        {
            return *cached;
        }
        self.hooks.purgeable_reads.fetch_add(1, Ordering::Relaxed);
        let read = volume::purgeable_free(dir);
        lock(&self.purgeable_cache).insert(dir.to_owned(), (Instant::now(), read));
        read
    }

    /// Refuses with [`DiskError::BelowFloor`] when any volume's usable free
    /// space is under its floor. `statvfs` (microseconds, reused for 5 s);
    /// on macOS one CoreFoundation call, reused for 5 s, only under twice
    /// the floor, which is why it runs inline on a create's task.
    pub fn check_floor(&self, idle: impl FnOnce() -> Option<Idle>) -> Result<(), DiskError> {
        let low = self
            .volumes(Purgeable::NearFloor)
            .into_iter()
            .find(|volume| volume.floor > 0 && volume.usable_free() < volume.floor);
        match low {
            None => Ok(()),
            Some(volume) => Err(DiskError::BelowFloor {
                free: volume.usable_free(),
                floor: volume.floor,
                volume: volume.path,
                idle: idle(),
            }),
        }
    }

    /// The budget on the workspace root's volume of `total` bytes.
    pub fn budget(&self, total: u64) -> u64 {
        self.config.budget.resolve(total, volume::auto_budget)
    }

    /// The machine figures as last walked: never walked on a request's
    /// path (the disk loop walks them at start and hourly); zero before the
    /// first walk.
    pub fn machine(&self) -> (u64, u64) {
        let cached = *lock(&self.machine);
        (cached.stores, cached.state)
    }

    /// Walks the stores and the state directory's own files now. Blocking.
    pub fn walk_machine(&self) -> (u64, u64) {
        let stores = allocated_under(&self.stores_dir, &[]);
        let skip: Vec<PathBuf> = std::fs::read_dir(&self.repos_dir)
            .into_iter()
            .flatten()
            .flatten()
            .flat_map(|repo| [repo.path().join("bases"), repo.path().join("ws")])
            .chain([self.stores_dir.clone()])
            .collect();
        let state = allocated_under(&self.state_dir, &skip);
        *lock(&self.machine) = Machine { stores, state };
        (stores, state)
    }

    /// Marks Reef's caches excluded from Time Machine, each once, unless
    /// `[disk] exclude_from_backups` is false. Blocking.
    pub fn exclude_from_backups(&self, db: &crate::db::Db) {
        if !self.config.exclude_from_backups || !cfg!(target_os = "macos") {
            return;
        }
        let bases = std::fs::read_dir(&self.repos_dir)
            .into_iter()
            .flatten()
            .flatten()
            .map(|repo| repo.path().join("bases"));
        for dir in self.backup_dirs.iter().cloned().chain(bases) {
            if let Err(error) = backup::exclude_once(db, &dir) {
                tracing::warn!(dir = %dir.display(), error = %reef_lib::error::ErrorChain(&error), "cannot record a Time Machine exclusion");
            }
        }
    }

    /// Test hook: every volume's free space, or `None` for the real one.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_free_for_tests(&self, free: Option<u64>) {
        *lock(&self.hooks.free) = free;
        lock(&self.space_cache).clear();
        lock(&self.purgeable_cache).clear();
    }

    /// Test hook: the measure interval, below its configured minimum.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_interval_for_tests(&self, interval: Duration) {
        *lock(&self.hooks.interval) = Some(interval);
    }

    /// Test hook: how long makes a workspace idle.
    #[cfg(any(test, feature = "testing"))]
    pub fn set_idle_after_for_tests(&self, idle_after: Duration) {
        *lock(&self.hooks.idle_after) = Some(idle_after);
    }

    /// Test hook: purgeable reads made.
    #[cfg(any(test, feature = "testing"))]
    pub fn purgeable_reads(&self) -> u64 {
        self.hooks.purgeable_reads.load(Ordering::Relaxed)
    }
}

fn with_forced(space: volume::Space, forced: Option<u64>) -> volume::Space {
    match forced {
        Some(free) => volume::Space { free, ..space },
        None => space,
    }
}

/// The level of a volume with `free` usable bytes and a floor of `floor`.
pub fn level_of(free: u64, floor: u64) -> Level {
    if floor == 0 {
        Level::Ok
    } else if free < floor {
        Level::Low
    } else if free < floor.saturating_mul(2) {
        Level::Warn
    } else {
        Level::Ok
    }
}

/// The level and reasons of `volumes` with Reef holding `reef` bytes
/// against `budget`.
pub fn assess(volumes: &[VolumeStatus], reef: u64, budget: u64) -> (Level, Vec<Reason>) {
    let mut reasons = Vec::new();
    for volume in volumes {
        let reason = match volume.level {
            Level::Low => Reason::BelowFloor,
            Level::Warn => Reason::NearFloor,
            Level::Ok => continue,
        };
        if !reasons.contains(&reason) {
            reasons.push(reason);
        }
    }
    if budget > 0 && reef > budget {
        reasons.push(Reason::OverBudget);
    }
    reasons.sort();
    let level = volumes
        .iter()
        .map(|volume| volume.level)
        .max()
        .unwrap_or(Level::Ok)
        .max(if reasons.contains(&Reason::OverBudget) {
            Level::Warn
        } else {
            Level::Ok
        });
    (level, reasons)
}

/// Allocated bytes under `dir`, not descending into `skip`, never
/// following symlinks; errors are skipped.
fn allocated_under(dir: &Path, skip: &[PathBuf]) -> u64 {
    use std::os::unix::fs::MetadataExt as _;

    walkdir::WalkDir::new(dir)
        .follow_links(false)
        .follow_root_links(false)
        .into_iter()
        .filter_entry(|entry| !skip.iter().any(|skip| entry.path() == skip))
        .take(usize::try_from(walk::Caps::DEFAULT.max_entries).unwrap_or(usize::MAX))
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .map(|metadata| metadata.blocks() * 512)
        .sum()
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(free: u64, floor: u64) -> VolumeStatus {
        VolumeStatus {
            path: PathBuf::from("/"),
            holds: vec!["workspaces"],
            space: volume::Space {
                total: 100 << 30,
                free,
            },
            purgeable_free: None,
            floor,
            level: level_of(free, floor),
        }
    }

    #[test]
    fn levels() {
        assert_eq!(level_of(10, 20), Level::Low);
        assert_eq!(level_of(30, 20), Level::Warn);
        assert_eq!(level_of(40, 20), Level::Ok);
        assert_eq!(level_of(0, 0), Level::Ok);
        assert_eq!(assess(&[volume(40, 20)], 5, 10), (Level::Ok, vec![]));
        assert_eq!(
            assess(&[volume(40, 20)], 11, 10),
            (Level::Warn, vec![Reason::OverBudget])
        );
        assert_eq!(
            assess(&[volume(30, 20), volume(10, 20)], 11, 10),
            (
                Level::Low,
                vec![Reason::BelowFloor, Reason::NearFloor, Reason::OverBudget]
            )
        );
        assert_eq!(assess(&[volume(40, 20)], 11, 0), (Level::Ok, vec![]));
    }

    #[test]
    fn usable_counts_purgeable() {
        let mut status = volume(10, 20);
        status.purgeable_free = Some(50);
        assert_eq!(status.usable_free(), 50);
        status.purgeable_free = Some(5);
        assert_eq!(status.usable_free(), 10);
    }

    fn disk(temp: &Path, section: DiskSection) -> Disk {
        let mut config = Config::with_defaults(crate::config::Paths::in_state_dir(temp).unwrap());
        config.file.disk = section;
        Disk::new(&config)
    }

    #[test]
    fn floor_and_fast_path() {
        let temp = tempfile::tempdir().unwrap();
        let disk = disk(
            temp.path(),
            DiskSection {
                floor: crate::config::DiskAmount::Bytes(crate::config::ByteSize(1 << 30)),
                ..DiskSection::default()
            },
        );
        disk.set_free_for_tests(Some(100 << 30));
        assert!(disk.check_floor(|| None).is_ok());
        assert_eq!(
            disk.purgeable_reads(),
            0,
            "above twice the floor: statvfs alone"
        );
        disk.set_free_for_tests(Some(1 << 20));
        let error = disk.check_floor(|| None).unwrap_err();
        assert!(matches!(error, DiskError::BelowFloor { floor, .. } if floor == 1 << 30));
        assert!(disk.purgeable_reads() > 0);
        assert_eq!(error.kind(), ErrorKind::Conflict);
        // Off.
        let off = self::disk(
            temp.path(),
            DiskSection {
                floor: crate::config::DiskAmount::Bytes(crate::config::ByteSize(0)),
                ..DiskSection::default()
            },
        );
        off.set_free_for_tests(Some(0));
        assert!(off.check_floor(|| None).is_ok());
    }

    #[test]
    fn auto_floor_on_a_real_volume() {
        let temp = tempfile::tempdir().unwrap();
        let disk = disk(temp.path(), DiskSection::default());
        let volumes = disk.volumes(Purgeable::NearFloor);
        assert_eq!(volumes.len(), 1, "{volumes:?}");
        assert_eq!(volumes[0].holds, ["workspaces", "state"]);
        assert_eq!(volumes[0].floor, volume::auto_floor(volumes[0].space.total));
    }

    #[test]
    fn scheduling() {
        let temp = tempfile::tempdir().unwrap();
        let disk = disk(temp.path(), DiskSection::default());
        disk.set_interval_for_tests(Duration::from_millis(50));
        let (a, b, c) = (
            WorkspaceId::generate(),
            WorkspaceId::generate(),
            WorkspaceId::generate(),
        );
        disk.track(a, true);
        disk.track(b, false);
        disk.track(c, false);
        let nobody = |_| false;
        assert_eq!(disk.due(&nobody), [a], "loaded dirty: due at once");
        disk.walk_started(a);
        disk.walked(a);
        disk.mark_dirty(b);
        assert!(
            disk.due(&nobody).is_empty(),
            "b was walked (created) under the interval ago"
        );
        std::thread::sleep(Duration::from_millis(60));
        assert_eq!(disk.due(&nobody), [b]);
        // Held and three intervals old: due though clean.
        std::thread::sleep(Duration::from_millis(100));
        let held = |id| id == c;
        let due = disk.due(&held);
        assert!(
            due.contains(&c) && due.contains(&b) && !due.contains(&a),
            "{due:?}"
        );
        disk.forget(b);
        assert!(!disk.due(&held).contains(&b));
        // A write during a walk survives it.
        disk.walk_started(c);
        disk.mark_dirty(c);
        disk.walked(c);
        assert!(disk.is_dirty(c));
        assert!(disk.written_within(c, Duration::from_secs(1)));
        assert!(!disk.written_within(a, Duration::from_secs(1)));
    }
}
