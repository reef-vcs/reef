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
//! 1. Create the state directory (`0700`) and take the single-instance lock.
//! 2. Open the database and apply migrations; write the pid file.
//! 3. Sweep leftovers: incomplete base cache entries (`*.tmp` under
//!    `repos/*/bases/`, R1.02); workspace trash (`*.trash-*` under
//!    `repos/*/ws/`, R1.07) and GC leftovers (`*.gc` under `repos/*/bases/`,
//!    R1.17) once those tickets land.
//! 4. Reconcile overlay mounts (R1.09; nothing until then).
//! 5. Start one repository thread per registered repository.
//! 6. Load active workspaces (R1.05; nothing until then).
//! 7. Apply the lease restart grace (R1.15; nothing until then).
//! 8. Replace a stale socket file, bind the socket (`0600`), and serve.
//!
//! # Shutdown
//!
//! [`DaemonHandle::shutdown`] cancels [`AppState::shutdown`]. The server stops
//! accepting connections and in-flight requests get [`SHUTDOWN_GRACE`]; then
//! the [`ShutdownHooks`] run (R1.13 and R1.15 add final snapshots), every
//! repository thread finishes its queued jobs and stops, the socket and pid
//! files are removed, and the lock is released. The lock file stays.

use std::fmt;
use std::io;
use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use reef_lib::jj::UserSettings;
use reef_lib::jj::settings::{SettingsError, user_settings};
use tokio::net::UnixListener;
use tokio::task::JoinHandle;
use tokio_stream::wrappers::UnixListenerStream;
use tokio_util::sync::CancellationToken;

use crate::backend::BaseCache;
use crate::config::{Config, ConfigError, default_identity};
use crate::db::{Db, DbError};
use crate::lock::{InstanceLock, LockError};
use crate::model::RepositoryRecord;
use crate::repo::RepoRegistry;
use crate::server;

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
    /// When the daemon started.
    pub started: Instant,
    /// Cancelled when shutdown begins. Long-running tasks select on it.
    pub shutdown: CancellationToken,
    /// Work to do after the server stops and before the repository threads
    /// do.
    pub hooks: ShutdownHooks,
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
            Self::Settings(error) => error.kind(),
            Self::SocketInUse { .. } => ErrorKind::Unavailable,
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

/// A running daemon. Dropping it without [`DaemonHandle::join`] begins
/// shutdown without waiting for it.
#[derive(Debug)]
pub struct DaemonHandle {
    state: Arc<AppState>,
    task: Option<JoinHandle<Result<(), StartError>>>,
}

impl Daemon {
    /// Runs the start sequence and returns once the socket is bound.
    pub async fn start(config: Config) -> Result<DaemonHandle, StartError> {
        let paths = config.paths.clone();
        create_private_dir(&paths.state_dir)?;
        let lock = InstanceLock::acquire(&paths.lock)?;
        let db = Arc::new(Db::open(&paths.db)?);
        std::fs::write(&paths.pid, format!("{}\n", std::process::id())).map_err(|source| {
            StartError::PidFile {
                path: paths.pid.clone(),
                source,
            }
        })?;
        let settings = match user_settings(&default_identity()) {
            Ok(settings) => settings,
            Err(error) => {
                remove_if_present(&paths.pid);
                return Err(error.into());
            }
        };
        let state = Arc::new(AppState {
            config,
            db,
            repos: Arc::new(RepoRegistry::new()),
            settings,
            started: Instant::now(),
            shutdown: CancellationToken::new(),
            hooks: ShutdownHooks::default(),
        });

        let listener = match bring_up(&state).await {
            Ok(listener) => listener,
            Err(error) => {
                let repos = Arc::clone(&state.repos);
                let _ = tokio::task::spawn_blocking(move || repos.close_all()).await;
                remove_if_present(&paths.pid);
                return Err(error);
            }
        };

        let router = server::router(Arc::clone(&state));
        let server = tokio::spawn(router.serve_with_incoming_shutdown(
            UnixListenerStream::new(listener),
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
        if self.task.is_some() {
            self.state.shutdown.cancel();
        }
    }
}

/// Steps 3 to 8 of the start sequence; returns the bound listener.
async fn bring_up(state: &Arc<AppState>) -> Result<UnixListener, StartError> {
    let paths = &state.config.paths;
    let records = state.db.repositories()?;

    // 3. Sweep leftovers.
    for record in &records {
        let cache = BaseCache::new(Arc::clone(&state.db), &paths.repo_dir(record.id), record.id);
        let repo = record.id;
        match tokio::task::spawn_blocking(move || cache.sweep_incomplete()).await {
            Ok(Ok(removed)) if !removed.is_empty() => {
                tracing::info!(%repo, removed = removed.len(), "swept incomplete base cache entries");
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                tracing::warn!(%repo, error = %ErrorChain(&error), "base cache sweep failed");
            }
            Err(error) => tracing::warn!(%repo, %error, "base cache sweep panicked"),
        }
    }

    // 4. Reconcile overlay mounts: R1.09.

    // 5. One repository thread per registered repository. A repository that
    //    fails to open is logged and skipped; the daemon serves the others.
    let opens = records.into_iter().map(|record| open_repo(state, record));
    futures::future::join_all(opens).await;

    // 6. Load active workspaces: R1.05.
    // 7. Lease restart grace: R1.15.

    // 8. Bind.
    bind(&paths.socket).await
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
    if let Some(parent) = socket.parent() {
        create_private_dir(parent)?;
    }
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
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600)).map_err(error)?;
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

/// Creates `dir` and missing parents with mode `0700`. An existing
/// directory is left as it is.
fn create_private_dir(dir: &Path) -> Result<(), StartError> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|source| StartError::CreateDir {
            path: dir.to_owned(),
            source,
        })
}

fn remove_if_present(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(path = %path.display(), %error, "cannot remove a file"),
    }
}
