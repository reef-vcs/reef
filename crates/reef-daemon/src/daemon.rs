//! Starting and stopping the daemon (ADR 0012).
//!
//! [`Daemon::start`] is the path the binary and the tests share. It runs on
//! the caller's tokio runtime, which must be multi-threaded, and returns once
//! the socket accepts connections.
//!
//! # Start sequence
//!
//! Later tickets extend this list in place, keeping its order:
//!
//! 1. Create the state directory and the socket's directory (`0700`), or
//!    refuse one another user owns and tighten one with group or other bits
//!    (RX.01, S-01); then take the single-instance lock. A root daemon logs
//!    a warning (S-04).
//! 2. Open the database and apply migrations; write the pid file. Apply the
//!    lease restart grace (R1.15): every unended lease gets at least
//!    `lease.restart_grace` from now, before anything can reclaim. If that
//!    fails, the daemon does not start.
//! 3. Sweep base cache leftovers: incomplete entries (`*.tmp` under
//!    `repos/*/bases/`, R1.02) and GC's (`*.gc`, R1.17). Then store builds
//!    a stop interrupted (R1.20): their unsealed rows go, and their
//!    `.building` directories are kept as failed; and store trash.
//! 4. Reconcile overlay mounts (Linux, R1.09): mount every `active` or
//!    `releasing` overlay workspace again whose mount is gone, over its
//!    upper directory; mark one that cannot be `attention = 'upper-missing'`
//!    or `'mount-failed'` (snapshots refuse an unmounted overlay); and
//!    unmount and remove overlay mounts under `repos/` and the workspace
//!    roots that no workspace owns. Before step 6, which reclaims a
//!    workspace whose root cannot be `stat`ed and watches every root.
//!    Then sweep workspace trash (`*.trash-*` and `.reef-probe-*` in each
//!    repository's workspace root, `repos/*/ws/` by default, R1.07).
//! 5. Start one repository thread per registered repository. Then rebuild
//!    each one's `snapshots` index from its operation log (R1.16) and log
//!    the report, before any snapshot can add a row.
//! 6. Load active workspaces ([`Manager::load_active`], R1.05): reopen each
//!    one's jj handle; reclaim one whose directory is gone, or after a
//!    restore (`meta.rematerialize_pending`, R1.18) rebuild it instead. Each
//!    is tracked by the snapshot scheduler, which then starts (R1.13).
//! 7. Replace a stale socket file, bind the socket (`0600`) in a directory
//!    the daemon owns (S-01), and serve.
//! 8. Start the lease expiry loop (R1.15), its first tick `lease.tick`
//!    later; after the bind, so a failed start leaves no loop running.
//! 9. Start the GC timer (#95) unless `gc.interval` is `0s`; its first run
//!    is 10 minutes later at the earliest.
//!
//! # Shutdown
//!
//! [`DaemonHandle::shutdown`] cancels [`AppState::shutdown`]. The server stops
//! accepting connections and in-flight requests get [`SHUTDOWN_GRACE`]; then
//! the [`ShutdownHooks`] run (a quiesce in effect resumes, R1.18; a timed
//! GC in progress finishes the repository it is on, #95; the snapshot
//! scheduler stops and its snapshots
//! in flight end, R1.13; the lease expiry loop stops after any reclaim in
//! progress, then every workspace with unsnapshotted events gets a
//! `shutdown` snapshot, R1.15; leases are left as they are), every
//! repository thread finishes its queued jobs and stops, the socket and pid
//! files are removed, and the lock is released. The lock file stays.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::UserSettings;
use reef_lib::jj::settings::SettingsError;
use reef_lib::telemetry::elapsed_ms;
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::backend::BaseCache;
use crate::backend::fs_util::sweep_trash;
use crate::config::{Config, ConfigError};
use crate::db::{Db, DbError};
use crate::lease::LeaseError;
use crate::lock::{InstanceLock, LockError};
use crate::maintenance::timer::{DaemonRunner, GcTimer};
use crate::model::{RepoId, RepositoryRecord};
use crate::repo::{OwnerPolicy, RepoRegistry};
use crate::server;
use crate::snapshot::index::{self, ReindexReport};
use crate::workspace::Manager;

/// How long in-flight requests may run after shutdown begins.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// How long the stale-socket check waits for a connection.
const SOCKET_PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// The daemon's shared state, handed to every service.
pub struct AppState {
    /// The configuration it started with.
    pub config: Config,
    /// The metadata database.
    pub db: Arc<Db>,
    /// The repository threads.
    pub repos: Arc<RepoRegistry>,
    /// The jj settings repositories are opened and written with.
    pub settings: UserSettings,
    /// The workspaces.
    pub manager: Arc<Manager>,
    /// When the daemon started.
    pub started: Instant,
    /// Cancelled when shutdown begins. Long-running tasks select on it.
    pub shutdown: CancellationToken,
    /// Reads over jj state (R2.02).
    pub reads: Arc<crate::read::Reads>,
    /// What subscriptions wait on: every repository's operations (R2.09).
    pub events: Arc<crate::subscribe::OpEvents>,
    /// The open subscriptions (R2.09).
    pub subscriptions: crate::subscribe::Subscriptions,
    /// The rings subscriptions are served from (R2.22).
    pub broker: Arc<crate::subscribe::broker::OpBroker>,
    /// The live presence entries (R2.10); the manager's.
    pub presence: Arc<crate::presence::PresenceHub>,
    /// Work to do after the server stops and before the repository threads
    /// do.
    pub hooks: ShutdownHooks,
    /// What each repository's start-time reindex of the `snapshots` index
    /// did (R1.16); absent for one whose reindex failed.
    pub reindexed: Mutex<HashMap<RepoId, ReindexReport>>,
    /// Held by GC (R1.17), backup and quiesce (R1.18).
    pub maintenance: crate::maintenance::Lock,
    /// The quiesce in effect, if any (R1.18).
    pub quiesce: crate::maintenance::quiesce::Quiescer,
}

impl fmt::Debug for AppState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppState")
            .field("paths", &self.config.paths)
            .field("repos", &self.repos)
            .field("started", &self.started)
            .field("shutdown", &self.shutdown)
            .finish_non_exhaustive()
    }
}

/// A shutdown hook: returns the future to await during shutdown.
pub type ShutdownHook = Box<dyn FnOnce() -> BoxFuture<'static, ()> + Send>;

/// Work run once, in registration order, after the server has stopped
/// serving and before the repository threads stop, so a hook can still send
/// them jobs.
#[derive(Default)]
pub struct ShutdownHooks {
    hooks: Mutex<Vec<(&'static str, ShutdownHook)>>,
}

impl fmt::Debug for ShutdownHooks {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let hooks = self.hooks.lock().unwrap_or_else(PoisonError::into_inner);
        f.debug_list()
            .entries(hooks.iter().map(|(name, _)| name))
            .finish()
    }
}

impl ShutdownHooks {
    /// Registers `hook` under `name`, which is logged when it runs.
    pub fn add(
        &self,
        name: &'static str,
        hook: impl FnOnce() -> BoxFuture<'static, ()> + Send + 'static,
    ) {
        self.hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push((name, Box::new(hook)));
    }

    async fn run(&self) {
        let hooks = std::mem::take(&mut *self.hooks.lock().unwrap_or_else(PoisonError::into_inner));
        for (name, hook) in hooks {
            tracing::debug!(hook = name, "running shutdown hook");
            hook().await;
        }
    }
}

/// Failure to start the daemon, or of the server while it ran.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StartError {
    /// The configuration did not resolve.
    #[error("invalid configuration")]
    Config(#[from] ConfigError),
    /// A directory the daemon keeps private is another user's (RX.01,
    /// S-01).
    #[error(
        "{} is owned by uid {uid}, not by uid {euid}, which runs the daemon; run the daemon as uid {uid}, or set {setting} to a directory of its own",
        path.display()
    )]
    NotOwned {
        /// The directory.
        path: PathBuf,
        /// Its owner.
        uid: u32,
        /// The daemon's effective uid.
        euid: u32,
        /// What to set instead, for the hint.
        setting: &'static str,
    },
    /// A directory could not be created.
    #[error("create {}", path.display())]
    CreateDir {
        /// The directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Another daemon holds the state directory, or the lock failed.
    #[error("acquire the single-instance lock")]
    Lock(#[from] LockError),
    /// The database could not be opened or migrated.
    #[error("open the database")]
    Db(#[from] DbError),
    /// The lease restart grace could not be applied; starting anyway could
    /// reclaim workspaces that were alive at shutdown.
    #[error("apply the lease restart grace")]
    RestartGrace(#[source] LeaseError),
    /// The jj settings could not be built.
    #[error("build the jj settings")]
    Settings(#[from] SettingsError),
    /// The pid file could not be written.
    #[error("write the pid file {}", path.display())]
    PidFile {
        /// The pid file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A daemon on another state directory answers on the socket.
    #[error("socket {} is in use by another daemon", path.display())]
    SocketInUse {
        /// The socket.
        path: PathBuf,
    },
    /// The socket could not be replaced, bound, or restricted.
    #[error("bind the socket {}", path.display())]
    Socket {
        /// The socket.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The server failed while running.
    #[error("the server failed")]
    Serve(#[source] tonic::transport::Error),
    /// The server or shutdown task panicked or was cancelled.
    #[error("a daemon task failed")]
    Task(#[source] tokio::task::JoinError),
}

impl Classify for StartError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Config(error) => error.kind(),
            Self::Lock(error) => error.kind(),
            Self::Db(error) => error.kind(),
            Self::RestartGrace(error) => error.kind(),
            Self::Settings(error) => error.kind(),
            Self::SocketInUse { .. } => ErrorKind::Unavailable,
            Self::NotOwned { .. } => ErrorKind::PermissionDenied,
            Self::CreateDir { source, .. }
            | Self::PidFile { source, .. }
            | Self::Socket { source, .. } => {
                if source.kind() == io::ErrorKind::PermissionDenied {
                    ErrorKind::PermissionDenied
                } else {
                    ErrorKind::Io
                }
            }
            Self::Serve(_) | Self::Task(_) => ErrorKind::Internal,
        }
    }
}

/// Starts daemons; see the module docs.
#[derive(Debug)]
pub struct Daemon;

/// A running daemon. Dropping it, or a [`DaemonHandle::join`] future that
/// has not finished, begins shutdown without waiting for it.
#[derive(Debug)]
pub struct DaemonHandle {
    state: Arc<AppState>,
    task: Option<JoinHandle<Result<(), StartError>>>,
}

impl Daemon {
    /// Runs the start sequence and returns once the socket is bound.
    pub async fn start(config: Config) -> Result<DaemonHandle, StartError> {
        let paths = config.paths.clone();
        // Before the lock, which creates a file in it.
        if let Some(mode) = private_state_dir(&paths.state_dir)? {
            log_tightened(&paths.state_dir, mode);
        }
        private_socket_dir(&paths.socket)?;
        warn_if_root();
        let lock = InstanceLock::acquire(&paths.lock)?;
        let db = Arc::new(Db::open(&paths.db)?);
        std::fs::write(&paths.pid, format!("{}\n", std::process::id())).map_err(|source| {
            StartError::PidFile {
                path: paths.pid.clone(),
                source,
            }
        })?;
        let settings = match config.jj_settings() {
            Ok(settings) => settings,
            Err(error) => {
                remove_if_present(&paths.pid);
                return Err(error.into());
            }
        };
        let owners = OwnerPolicy::from_env(
            config.file.repo.allow_foreign_owner,
            paths.config_file.clone(),
        );
        tracing::info!(
            euid = owners.euid,
            sudo_uid = owners.sudo_uid,
            allow_foreign_owner = owners.allow_foreign,
            "registration accepts repositories these uids own"
        );
        let events = Arc::new(crate::subscribe::OpEvents::default());
        let repos = Arc::new(RepoRegistry::with_owners(owners).with_events(Arc::clone(&events)));
        let manager = Manager::new(
            config.clone(),
            Arc::clone(&db),
            Arc::clone(&repos),
            settings.clone(),
        );
        let shutdown = CancellationToken::new();
        let broker = Arc::new(crate::subscribe::broker::OpBroker::new(&config.subscribe));
        let state = Arc::new(AppState {
            config,
            db,
            repos,
            settings,
            manager: Arc::clone(&manager),
            started: Instant::now(),
            reads: Arc::new(crate::read::Reads::new(shutdown.clone())),
            events,
            subscriptions: crate::subscribe::Subscriptions::default(),
            broker,
            presence: Arc::clone(manager.presence()),
            shutdown,
            hooks: ShutdownHooks::default(),
            reindexed: Mutex::new(HashMap::new()),
            maintenance: crate::maintenance::Lock::new(),
            quiesce: crate::maintenance::quiesce::Quiescer::default(),
        });

        // Before any snapshot can run (R2.10).
        state
            .manager
            .scheduler()
            .on_snapshot(crate::presence::publish::editing_hook(Arc::downgrade(
                &state,
            )));
        let listener = match bring_up(&state).await {
            Ok(listener) => listener,
            Err(error) => {
                let repos = Arc::clone(&state.repos);
                let _ = tokio::task::spawn_blocking(move || repos.close_all()).await;
                remove_if_present(&paths.pid);
                return Err(error);
            }
        };

        Arc::clone(&state.events).poll(state.config.subscribe.outside_poll, state.shutdown.clone());
        crate::subscribe::broker::supervise(&state);
        state.presence.start(state.shutdown.clone());
        let router = server::router(Arc::clone(&state));
        let server = tokio::spawn(router.serve_with_incoming_shutdown(
            crate::limits::Accepting::new(listener),
            state.shutdown.clone().cancelled_owned(),
        ));
        tracing::info!(
            version = env!("CARGO_PKG_VERSION"),
            pid = std::process::id(),
            socket = %paths.socket.display(),
            state_dir = %paths.state_dir.display(),
            "daemon started"
        );
        let task = tokio::spawn(supervise(Arc::clone(&state), server, lock));
        Ok(DaemonHandle {
            state,
            task: Some(task),
        })
    }
}

impl DaemonHandle {
    /// The socket the daemon listens on.
    pub fn socket_path(&self) -> &Path {
        &self.state.config.paths.socket
    }

    /// The daemon's shared state.
    pub fn state(&self) -> &Arc<AppState> {
        &self.state
    }

    /// Begins shutdown. Returns at once; [`DaemonHandle::join`] waits for it.
    pub fn shutdown(&self) {
        self.state.shutdown.cancel();
    }

    /// Waits for the daemon to stop, after [`DaemonHandle::shutdown`] or a
    /// server failure, and returns the server's result.
    pub async fn join(mut self) -> Result<(), StartError> {
        match self.task.take() {
            Some(task) => task.await.map_err(StartError::Task)?,
            None => Ok(()),
        }
    }
}

impl Drop for DaemonHandle {
    fn drop(&mut self) {
        // Unconditional: `join` takes the task when first polled, so a join
        // future dropped early (a timeout) must still stop the daemon.
        // Cancelling a stopped daemon's token is a no-op.
        self.state.shutdown.cancel();
    }
}

/// The rest of step 2, and steps 3 to 8 of the start sequence; returns the
/// bound listener.
async fn bring_up(state: &Arc<AppState>) -> Result<UnixListener, StartError> {
    let paths = &state.config.paths;
    for warning in state.config.warnings() {
        tracing::warn!(config = %paths.config_file.display(), "{warning}");
    }
    // 2. The lease restart grace, before anything can reclaim.
    state
        .manager
        .leases()
        .apply_restart_grace()
        .map_err(StartError::RestartGrace)?;
    let records = state.db.repositories()?;

    // 3. Sweep base cache leftovers.
    for record in &records {
        let cache = BaseCache::new(Arc::clone(&state.db), &paths.repo_dir(record.id), record.id);
        let repo = record.id;
        match tokio::task::spawn_blocking(move || cache.sweep_incomplete()).await {
            Ok(Ok(removed)) if !removed.is_empty() => {
                tracing::info!(
                    %repo,
                    removed_count = removed.len(),
                    "swept incomplete base cache entries"
                );
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!(%repo, error = %ErrorChain(&error), "base cache sweep failed");
            }
            Err(error) => tracing::warn!(%repo, %error, "base cache sweep panicked"),
        }
    }

    //    Then interrupted store builds and store trash.
    sweep_stores(state).await;

    // 4. Reconcile overlay mounts, then sweep workspace trash: a probe
    //    directory a crash left mounted is trash only once unmounted.
    #[cfg(target_os = "linux")]
    reconcile_overlays(state, &records).await;
    for record in &records {
        let repo = record.id;
        let ws_root = state.config.workspace_root(record.id);
        match tokio::task::spawn_blocking(move || sweep_trash(&ws_root)).await {
            Ok(Ok(0)) => {}
            Ok(Ok(removed)) => {
                tracing::info!(%repo, removed_count = removed, "swept workspace trash");
            }
            Ok(Err(error)) => tracing::warn!(%repo, %error, "workspace trash sweep failed"),
            Err(error) => tracing::warn!(%repo, %error, "workspace trash sweep panicked"),
        }
    }

    // 5. One repository thread per registered repository. A repository that
    //    fails to open is logged and skipped; the daemon serves the others.
    let opens = records.into_iter().map(|record| open_repo(state, record));
    futures::future::join_all(opens).await;
    //    Then count the tracked files of repositories registered before
    //    R1.14 and log each one's watcher selection.
    match state.db.repositories() {
        Ok(records) => {
            let open = records
                .into_iter()
                .filter(|record| state.repos.get(record.id).is_some())
                .map(|record| state.manager.prepare_watcher(record));
            futures::future::join_all(open).await;
        }
        Err(error) => tracing::error!(error = %ErrorChain(&error), "cannot list repositories"),
    }
    //    Then undo the creates a crash interrupted (R1.23): directories,
    //    jj workspaces, and working-copy state that no row names.
    sweep_abandoned_creates(state).await;
    //    Then rebuild each open repository's snapshot index, before the
    //    scheduler can record new rows.
    let reindexes = state
        .repos
        .ids()
        .into_iter()
        .map(|id| reindex_repo(state, id));
    futures::future::join_all(reindexes).await;

    // 6. Load active workspaces. A database failure here is logged; the
    //    daemon still serves registration and the database's view.
    match state.manager.load_active().await {
        Ok(0) => {}
        Ok(loaded) => tracing::info!(loaded_count = loaded, "active workspaces loaded"),
        Err(error) => tracing::error!(error = %ErrorChain(&error), "cannot load active workspaces"),
    }
    //    Then start the snapshot scheduler: watcher events, the fallback
    //    timer, and the summary line. Workspaces were tracked as they loaded.
    //    A quiesce in effect at shutdown resumes first (R1.18), so the hooks
    //    after it never wait on a paused repository thread.
    let weak = Arc::downgrade(state);
    state.hooks.add("quiesce", move || {
        Box::pin(async move {
            if let Some(state) = weak.upgrade() {
                crate::maintenance::quiesce::resume_any(&state);
            }
        })
    });
    //    A timed GC in progress finishes its repository before the
    //    scheduler stops, so its snapshots still work. The timer starts in
    //    step 9.
    let gc_timer = Arc::new(GcTimer::new());
    let joined = Arc::clone(&gc_timer);
    state.hooks.add("gc timer", move || {
        Box::pin(async move { joined.join().await })
    });
    state.manager.scheduler().start(&state.shutdown);
    let manager = Arc::clone(&state.manager);
    state.hooks.add("snapshot scheduler", move || {
        Box::pin(async move { manager.scheduler().stop().await })
    });
    // 7. Bind.
    let listener = bind(&paths.socket).await?;

    start_loops(state, &gc_timer);
    Ok(listener)
}

/// Steps 8 and 9, after the bind: the lease expiry loop with its hooks,
/// and the GC timer, whose hook step 6 registered.
fn start_loops(state: &Arc<AppState>, gc_timer: &GcTimer) {
    // 8. The lease expiry loop. Its hook runs after the scheduler's, and the
    //    shutdown snapshots after both, so no reclaim or watcher snapshot
    //    races them.
    let expiry = Arc::clone(state.manager.leases()).start_expiry_loop();
    let leases = Arc::clone(state.manager.leases());
    state.hooks.add("lease expiry loop", move || {
        Box::pin(async move {
            leases.stop();
            if let Err(error) = expiry.await {
                tracing::error!(%error, "the lease expiry loop panicked");
            }
        })
    });
    let manager = Arc::clone(&state.manager);
    state.hooks.add("shutdown snapshots", move || {
        Box::pin(async move {
            manager.shutdown_snapshots().await;
        })
    });

    // 9. The GC timer.
    gc_timer.start(
        state.config.file.gc.interval,
        state.config.gc_timer,
        DaemonRunner(Arc::downgrade(state)),
        state.shutdown.clone(),
    );
}

/// The end of step 5: [`crate::workspace::Manager::sweep_abandoned_creates`]
/// for every open repository.
async fn sweep_abandoned_creates(state: &AppState) {
    for id in state.repos.ids() {
        let undone = state.manager.sweep_abandoned_creates(id).await;
        if undone > 0 {
            tracing::info!(repo = %id, undone_count = undone, "undid interrupted creates");
        }
    }
}

/// The end of step 3: the store builds a stop interrupted, and store trash
/// (R1.20). Failures are logged.
async fn sweep_stores(state: &AppState) {
    let stores = Arc::clone(state.manager.stores());
    match tokio::task::spawn_blocking(move || (stores.sweep_interrupted(), stores.sweep_trash()))
        .await
    {
        Ok((0, 0)) => {}
        Ok((interrupted, trash)) => tracing::info!(
            interrupted_count = interrupted,
            trash_count = trash,
            "swept store leftovers"
        ),
        Err(error) => tracing::warn!(%error, "store sweep panicked"),
    }
}

/// Step 4: re-establishes or cleans up overlay mounts, records the
/// attention of workspaces left unmounted (and clears it from ones mounted
/// again), and logs the report. Failures are logged; the daemon starts
/// regardless.
#[cfg(target_os = "linux")]
async fn reconcile_overlays(state: &Arc<AppState>, records: &[RepositoryRecord]) {
    use crate::backend::overlay::{OverlayWorkspace, ReconcileReport, reconcile_mounts};
    use crate::backend::{BackendKind, WorkspacePaths};
    use crate::model::{Attention, WorkspaceState};

    let rows = match state
        .db
        .workspaces(None, &[WorkspaceState::Active, WorkspaceState::Releasing])
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::error!(error = %ErrorChain(&error), "cannot list workspaces to reconcile overlay mounts");
            return;
        }
    };
    let paths = &state.config.paths;
    let mut attention = HashMap::new();
    let workspaces: Vec<OverlayWorkspace> = rows
        .into_iter()
        .filter_map(|record| {
            let kind = record.backend.parse::<BackendKind>().ok().filter(|kind| {
                matches!(kind, BackendKind::Overlayfs | BackendKind::FuseOverlayfs)
            })?;
            let cache = BaseCache::new(
                Arc::clone(&state.db),
                &paths.repo_dir(record.repo_id),
                record.repo_id,
            );
            let lower = cache
                .get(&record.base_tree_id)
                .map(|entry| entry.map(|entry| entry.path))
                .map_err(|error| ErrorChain(&error).to_string());
            attention.insert(record.id, record.attention);
            Some(OverlayWorkspace {
                paths: WorkspacePaths::at(record.id, record.path, record.state_dir),
                kind,
                lower,
            })
        })
        .collect();
    let mut scan_dirs = vec![paths.repos.clone()];
    scan_dirs.extend(
        records
            .iter()
            .map(|record| state.config.workspace_root(record.id)),
    );
    let overlay = state.config.file.workspace.overlay();
    let begun = Instant::now();
    let report = match tokio::task::spawn_blocking(move || {
        reconcile_mounts(&scan_dirs, &workspaces, &overlay)
    })
    .await
    {
        Ok(Ok(report)) => report,
        Ok(Err(error)) => {
            tracing::error!(error = %ErrorChain(&error), "cannot reconcile overlay mounts");
            return;
        }
        Err(error) => {
            tracing::error!(%error, "reconciling overlay mounts panicked");
            return;
        }
    };
    let set = |id, value: Option<Attention>| {
        if let Err(error) = state.db.set_workspace_attention(id, value, None) {
            tracing::error!(workspace_id = %id, error = %ErrorChain(&error), "cannot record the workspace's attention");
        }
    };
    for &id in &report.upper_missing {
        tracing::error!(workspace_id = %id, "the overlay workspace's upper directory is missing; it is left unmounted");
        set(id, Some(Attention::UpperMissing));
    }
    for (id, reason) in &report.failed {
        tracing::error!(workspace_id = %id, %reason, "cannot mount the overlay workspace again; it is left unmounted");
        set(*id, Some(Attention::MountFailed));
    }
    for &id in report.mounted.iter().chain(&report.remounted) {
        if matches!(
            attention.get(&id),
            Some(Some(Attention::UpperMissing | Attention::MountFailed))
        ) {
            set(id, None);
        }
    }
    // Quiet when there was nothing to do.
    if report != ReconcileReport::default() {
        tracing::info!(
            mounted_count = report.mounted.len(),
            remounted_count = report.remounted.len(),
            cleaned_count = report.cleaned.len(),
            attention_count = report.upper_missing.len() + report.failed.len(),
            elapsed_ms = elapsed_ms(begun),
            "overlay mounts reconciled"
        );
    }
}

/// Rebuilds one repository's `snapshots` index from its operation log and
/// logs the report. A failure is logged; the index keeps its rows, and new
/// snapshots still add theirs.
async fn reindex_repo(state: &Arc<AppState>, id: RepoId) {
    let Some(handle) = state.repos.get(id) else {
        return;
    };
    let limit = usize::try_from(state.config.file.gc.reindex_limit).unwrap_or(usize::MAX);
    let begun = Instant::now();
    match index::reindex(&state.db, &handle, id, limit).await {
        Ok(report) => {
            tracing::info!(
                repo = %id,
                added_count = report.added,
                removed_count = report.removed,
                skipped_count = report.skipped,
                walked_count = report.walked,
                truncated = report.truncated,
                elapsed_ms = elapsed_ms(begun),
                "snapshot index reconciled with the operation log"
            );
            if report.truncated {
                tracing::warn!(
                    repo = %id,
                    reindex_limit = limit,
                    "the operation log is longer than gc.reindex_limit; older snapshots were not \
                     checked and no rows were removed"
                );
            }
            state
                .reindexed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id, report);
        }
        Err(error) => tracing::error!(
            repo = %id,
            error = %ErrorChain(&error),
            "cannot reconcile the snapshot index with the operation log"
        ),
    }
}

async fn open_repo(state: &Arc<AppState>, record: RepositoryRecord) {
    let repos = Arc::clone(&state.repos);
    let settings = state.settings.clone();
    let id = record.id;
    let root = record.root.clone();
    match tokio::task::spawn_blocking(move || repos.open(&record, settings)).await {
        Ok(Ok(_)) => tracing::debug!(repo = %id, root = %root.display(), "repository opened"),
        Ok(Err(error)) => tracing::error!(
            repo = %id,
            root = %root.display(),
            error = %ErrorChain(&error),
            "cannot open a registered repository; it is not served until the daemon restarts"
        ),
        Err(error) => tracing::error!(repo = %id, %error, "opening a repository panicked"),
    }
}

/// Binds the socket, replacing a stale socket file. Called with the lock
/// held, so no daemon on this state directory owns the file; a daemon on
/// another state directory configured with the same socket is detected by
/// connecting first.
async fn bind(socket: &Path) -> Result<UnixListener, StartError> {
    let error = |source| StartError::Socket {
        path: socket.to_owned(),
        source,
    };
    // Checked at start already; created again if it went meanwhile.
    private_socket_dir(socket)?;
    let probe = tokio::time::timeout(
        SOCKET_PROBE_TIMEOUT,
        tokio::net::UnixStream::connect(socket),
    );
    if matches!(probe.await, Ok(Ok(_)) | Err(_)) {
        return Err(StartError::SocketInUse {
            path: socket.to_owned(),
        });
    }
    match std::fs::remove_file(socket) {
        Ok(()) => tracing::info!(socket = %socket.display(), "removed a stale socket file"),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(error(e)),
    }
    let listener = UnixListener::bind(socket).map_err(error)?;
    // The state directory is 0700 already; this also covers a socket outside
    // it (`REEF_SOCKET`, `$XDG_RUNTIME_DIR`).
    if let Err(e) = std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)) {
        drop(listener);
        remove_if_present(socket);
        return Err(error(e));
    }
    Ok(listener)
}

/// Waits for shutdown or a server failure, then tears down.
async fn supervise(
    state: Arc<AppState>,
    mut server: JoinHandle<Result<(), tonic::transport::Error>>,
    lock: InstanceLock,
) -> Result<(), StartError> {
    let stopped_by_itself = tokio::select! {
        joined = &mut server => Some(joined),
        () = state.shutdown.cancelled() => None,
    };
    let outcome = if let Some(joined) = stopped_by_itself {
        tracing::error!("the server stopped by itself; shutting down");
        joined
    } else {
        tracing::info!("shutting down");
        if let Ok(joined) = tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
            joined
        } else {
            tracing::warn!(
                grace_ms = u64::try_from(SHUTDOWN_GRACE.as_millis()).unwrap_or(u64::MAX),
                "in-flight requests did not finish in time; dropping them"
            );
            server.abort();
            Ok(Ok(()))
        }
    };
    state.shutdown.cancel();
    state.hooks.run().await;
    let repos = Arc::clone(&state.repos);
    if let Err(error) = tokio::task::spawn_blocking(move || repos.close_all()).await {
        tracing::error!(%error, "closing repository threads panicked");
    }
    let paths = &state.config.paths;
    remove_if_present(&paths.socket);
    remove_if_present(&paths.pid);
    drop(lock);
    tracing::info!("daemon stopped");
    outcome
        .map_err(StartError::Task)?
        .map_err(StartError::Serve)
}

/// What a [`StartError::NotOwned`] for the state directory says to set.
const STATE_SETTING: &str = "REEF_STATE_DIR (for example /var/lib/reef)";
/// The same for the socket's directory.
const SOCKET_SETTING: &str = "REEF_SOCKET";

/// Why [`ensure_private_dir`] refused or failed.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum PrivateDirError {
    /// The directory is another user's.
    #[error("{} is owned by uid {uid}, not by uid {euid}", path.display())]
    NotOwned {
        /// The directory.
        path: PathBuf,
        /// Its owner.
        uid: u32,
        /// The daemon's effective uid.
        euid: u32,
    },
    /// It could not be created, read, or restricted.
    #[error("create {}", path.display())]
    Io {
        /// The directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

/// Makes `dir` a directory only the daemon's user can use (RX.01, S-01):
/// an existing one owned by another uid is refused, root included (two uids
/// sharing a state directory never works); one with any group or other bit
/// is `chmod`ed `0700`, and its old mode returned for the caller to log. A
/// missing one is created with missing parents, `0700`, when `create` is
/// set, and is otherwise left missing.
pub(crate) fn ensure_private_dir(dir: &Path, create: bool) -> Result<Option<u32>, PrivateDirError> {
    let io_error = |source| PrivateDirError::Io {
        path: dir.to_owned(),
        source,
    };
    let metadata = match std::fs::metadata(dir) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if create {
                std::fs::DirBuilder::new()
                    .recursive(true)
                    .mode(0o700)
                    .create(dir)
                    .map_err(io_error)?;
            }
            return Ok(None);
        }
        Err(error) => return Err(io_error(error)),
    };
    if !metadata.is_dir() {
        return Err(io_error(io::ErrorKind::NotADirectory.into()));
    }
    let euid = nix::unistd::geteuid().as_raw();
    if metadata.uid() != euid {
        return Err(PrivateDirError::NotOwned {
            path: dir.to_owned(),
            uid: metadata.uid(),
            euid,
        });
    }
    let mode = metadata.mode() & 0o7777;
    let group_or_other = mode & 0o077;
    if group_or_other == 0 {
        return Ok(None);
    }
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io_error)?;
    Ok(Some(mode))
}

/// Logs that [`ensure_private_dir`] tightened `dir` from `mode`.
pub(crate) fn log_tightened(dir: &Path, mode: u32) {
    tracing::warn!(
        path = %dir.display(),
        mode = format!("{mode:04o}"),
        "a private directory had group or other permissions; changed it to 0700"
    );
}

/// [`ensure_private_dir`] for the state directory, creating it; the binary
/// calls it before logging starts, so the caller logs a tightened mode.
pub(crate) fn private_state_dir(dir: &Path) -> Result<Option<u32>, StartError> {
    ensure_private_dir(dir, true).map_err(|error| start_error(error, STATE_SETTING))
}

/// [`ensure_private_dir`] for the socket's directory, creating it, and
/// logging a tightened mode.
fn private_socket_dir(socket: &Path) -> Result<(), StartError> {
    let Some(parent) = socket.parent() else {
        return Ok(());
    };
    match ensure_private_dir(parent, true) {
        Ok(Some(mode)) => log_tightened(parent, mode),
        Ok(None) => {}
        Err(error) => return Err(start_error(error, SOCKET_SETTING)),
    }
    Ok(())
}

fn start_error(error: PrivateDirError, setting: &'static str) -> StartError {
    match error {
        PrivateDirError::NotOwned { path, uid, euid } => StartError::NotOwned {
            path,
            uid,
            euid,
            setting,
        },
        PrivateDirError::Io { path, source } => StartError::CreateDir { path, source },
    }
}

/// The warning a root daemon logs at start (RX.01, S-04).
pub const ROOT_WARNING: &str = "running as root: every client of this socket is root, and store builds run repository commands as root; the kernel overlay tier is meant for a VM or container dedicated to the agents";

fn warn_if_root() {
    if nix::unistd::geteuid().is_root() {
        tracing::warn!("{ROOT_WARNING}");
    }
}

fn remove_if_present(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(path = %path.display(), %error, "cannot remove a file"),
    }
}
