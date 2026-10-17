//! [`WatchmanWatcher`]: one Watchman subscription per workspace (R1.14,
//! ADR 0023).
//!
//! This is only the trigger. A Watchman workspace's working copy is loaded
//! with jj's `fsmonitor.backend = "watchman"`, so each snapshot asks
//! Watchman itself which paths changed since the clock in the working
//! copy's state, and scans only those. The two are independent: a lost
//! trigger costs latency until the fallback timer, never correctness.
//!
//! jj connects to Watchman again for every snapshot, and without
//! `WATCHMAN_SOCK` each connection first runs `watchman get-sockname` to
//! find the socket: 135 ms of CLI start-up per snapshot on an M-series Mac,
//! more than a full scan of 10,000 files. [`export_socket`] sets
//! `WATCHMAN_SOCK` once at daemon start instead.
//!
//! Every subscription shares one `watchman_client::Client`. Each runs in its
//! own task: subscribe, then turn each batch Watchman sends into a
//! [`WatchEvent`] once `watcher.debounce` passes with no further batch. When
//! the subscription is cancelled or the connection is lost, the task
//! reports the workspace unhealthy (`attention = 'watch-failed'`, on the
//! fallback timer) and subscribes again after 1 s, 2 s, 4 s, and so on up
//! to 30 s, until it succeeds or the workspace is unwatched.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use reef_lib::error::ErrorChain;
use reef_lib::jj::watchman_client::prelude::{
    CanonicalPath, Client, Connector, DirNameTerm, Expr, NameOnly, NameTerm, SubscribeRequest,
};
use reef_lib::jj::watchman_client::{self, Subscription, SubscriptionData};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{HealthHook, WatchError, WatchEvent, Watcher, WatcherKind, watchman_on_path};
use crate::model::WorkspaceId;

/// The first reconnect delay; it doubles with each failed attempt.
const BACKOFF_FIRST: Duration = Duration::from_secs(1);

/// The longest reconnect delay.
const BACKOFF_MAX: Duration = Duration::from_secs(30);

/// How long an unwatched workspace's task waits for `unsubscribe`.
const UNSUBSCRIBE_TIMEOUT: Duration = Duration::from_secs(2);

/// How long `watchman get-sockname` may take, including starting the
/// server.
const SOCKNAME_TIMEOUT: Duration = Duration::from_secs(30);

/// The environment variable `watchman_client` (and so jj) connects to
/// instead of running `watchman get-sockname`.
pub const SOCKET_ENV: &str = "WATCHMAN_SOCK";

/// What [`export_socket`] did.
#[derive(Debug)]
pub enum SocketExport {
    /// `watcher.backend` is `notify`, or `watchman` is not on `PATH`.
    NotNeeded,
    /// `WATCHMAN_SOCK` was already set; it is left alone.
    Inherited(OsString),
    /// `WATCHMAN_SOCK` is now this socket.
    Exported(PathBuf),
    /// The socket could not be found; jj discovers it on every snapshot.
    Failed(String),
}

/// Unless `backend` is `notify` or `watchman` is not on `PATH`, runs
/// `watchman get-sockname` once (starting the Watchman server if it is not
/// running) and sets `WATCHMAN_SOCK` to the answer, so jj's connection for
/// each snapshot skips discovery. The daemon's own connections still
/// discover the socket, which starts the server again after it stopped.
///
/// # Safety
///
/// Changes the process environment: call it before the process starts any
/// other thread, as [`crate::run_main`] does, so nothing reads the
/// environment concurrently.
#[allow(unsafe_code, reason = "setenv before any other thread exists")]
pub unsafe fn export_socket(backend: crate::config::WatcherBackend) -> SocketExport {
    if backend == crate::config::WatcherBackend::Notify || !watchman_on_path() {
        return SocketExport::NotNeeded;
    }
    if let Some(existing) = std::env::var_os(SOCKET_ENV) {
        return SocketExport::Inherited(existing);
    }
    match sockname() {
        Ok(socket) => {
            // SAFETY: the caller guarantees no other thread exists.
            unsafe { std::env::set_var(SOCKET_ENV, &socket) };
            SocketExport::Exported(socket)
        }
        Err(error) => SocketExport::Failed(error),
    }
}

/// What `watchman get-sockname` prints, in JSON.
#[derive(serde::Deserialize)]
struct SocknameAnswer {
    sockname: Option<PathBuf>,
    error: Option<String>,
}

/// Runs `watchman get-sockname` without `WATCHMAN_SOCK`, so the CLI does its
/// own discovery and starts the server if it is not running. Blocks for up
/// to [`SOCKNAME_TIMEOUT`].
fn sockname() -> Result<PathBuf, String> {
    let mut child = Command::new("watchman")
        .args(["--output-encoding=json", "--no-pretty", "get-sockname"])
        .env_remove(SOCKET_ENV)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("run `watchman get-sockname`: {error}"))?;
    let deadline = Instant::now() + SOCKNAME_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "`watchman get-sockname` did not answer within {SOCKNAME_TIMEOUT:?}"
                ));
            }
            Err(error) => return Err(format!("wait for `watchman get-sockname`: {error}")),
        }
    }
    let output = child
        .wait_with_output()
        .map_err(|error| format!("read `watchman get-sockname`: {error}"))?;
    let answer: SocknameAnswer = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "`watchman get-sockname` answered {:?} ({error}); stderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr).trim()
        )
    })?;
    match (answer.sockname, answer.error) {
        (_, Some(error)) => Err(format!("`watchman get-sockname`: {error}")),
        (Some(socket), None) => Ok(socket),
        (None, None) => Err("`watchman get-sockname` named no socket".to_owned()),
    }
}

/// The Watchman watcher: a subscription task per workspace, one client for
/// all of them.
pub struct WatchmanWatcher {
    shared: Arc<Shared>,
    runtime: tokio::runtime::Handle,
    tasks: Mutex<HashMap<WorkspaceId, CancellationToken>>,
}

impl std::fmt::Debug for WatchmanWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchmanWatcher")
            .field("subscriptions", &lock(&self.tasks).len())
            .finish_non_exhaustive()
    }
}

/// What every subscription task shares.
struct Shared {
    /// The connection, made by the first task that needs it and dropped
    /// when one finds it lost.
    client: tokio::sync::Mutex<Option<Arc<Client>>>,
    events: mpsc::Sender<WatchEvent>,
    debounce: Duration,
    health: HealthHook,
}

impl WatchmanWatcher {
    /// A watcher with nothing watched yet, on the current tokio runtime.
    /// Nothing connects to Watchman until the first [`Watcher::watch`].
    /// Batches arrive `debounce` after the last change in them; `health`
    /// hears when each workspace's subscription comes up or goes down.
    pub fn new(
        debounce: Duration,
        events: mpsc::Sender<WatchEvent>,
        health: HealthHook,
    ) -> Result<Self, WatchError> {
        let runtime =
            tokio::runtime::Handle::try_current().map_err(|_| WatchError::NoRuntime("watchman"))?;
        Ok(Self {
            shared: Arc::new(Shared {
                client: tokio::sync::Mutex::new(None),
                events,
                debounce,
                health,
            }),
            runtime,
            tasks: Mutex::new(HashMap::new()),
        })
    }

    /// Whether a `watchman` executable is on `PATH`, which is where
    /// `watchman_client` (and so jj) finds it.
    pub fn binary_on_path() -> bool {
        watchman_on_path()
    }
}

impl Drop for WatchmanWatcher {
    fn drop(&mut self) {
        for (_, stop) in lock(&self.tasks).drain() {
            stop.cancel();
        }
    }
}

impl Watcher for WatchmanWatcher {
    fn kind(&self) -> WatcherKind {
        WatcherKind::Watchman
    }

    fn reports_health(&self) -> bool {
        true
    }

    fn watch(
        &self,
        workspace: WorkspaceId,
        root: &Path,
        skip: Vec<PathBuf>,
    ) -> Result<(), WatchError> {
        let stop = CancellationToken::new();
        if let Some(old) = lock(&self.tasks).insert(workspace, stop.clone()) {
            old.cancel();
        }
        let task = Task {
            shared: Arc::clone(&self.shared),
            workspace,
            root: root.to_owned(),
            skip,
            stop,
        };
        self.runtime.spawn(task.run());
        Ok(())
    }

    fn unwatch(&self, workspace: WorkspaceId) -> Result<(), WatchError> {
        if let Some(stop) = lock(&self.tasks).remove(&workspace) {
            stop.cancel();
        }
        Ok(())
    }
}

/// Failure to set up a subscription.
#[derive(Debug, thiserror::Error)]
enum SubscribeError {
    #[error("find the watchman socket: {0}")]
    Discover(String),
    #[error("connect to watchman")]
    Connect(#[source] watchman_client::Error),
    #[error("resolve {}", .0.display())]
    Canonicalize(PathBuf, #[source] std::io::Error),
    #[error("watch {}", .0.display())]
    Resolve(PathBuf, #[source] watchman_client::Error),
    #[error("subscribe to {}", .0.display())]
    Subscribe(PathBuf, #[source] watchman_client::Error),
}

impl SubscribeError {
    /// Whether the connection itself failed, so the next attempt should
    /// make a new one.
    fn lost_connection(&self) -> bool {
        match self {
            Self::Discover(_) | Self::Connect(_) => true,
            Self::Canonicalize(..) => false,
            Self::Resolve(_, error) | Self::Subscribe(_, error) => is_connection_error(error),
        }
    }
}

fn is_connection_error(error: &watchman_client::Error) -> bool {
    matches!(
        error,
        watchman_client::Error::ConnectionError(_)
            | watchman_client::Error::ConnectionLost(_)
            | watchman_client::Error::ConnectionDiscovery { .. }
            | watchman_client::Error::Connect { .. }
    )
}

/// Why a subscription stopped delivering.
enum Ended {
    /// The workspace was unwatched.
    Stopped,
    /// Watchman cancelled it (`lost_connection` false: the connection is
    /// still good), or the connection failed.
    Lost {
        reason: String,
        lost_connection: bool,
    },
}

impl Shared {
    /// The shared client, connecting first if there is none. Connecting
    /// discovers the socket afresh (ignoring `WATCHMAN_SOCK`), which starts
    /// the Watchman server if it stopped.
    async fn client(&self) -> Result<Arc<Client>, SubscribeError> {
        let mut slot = self.client.lock().await;
        if let Some(client) = slot.as_ref() {
            return Ok(Arc::clone(client));
        }
        let socket = tokio::task::spawn_blocking(sockname)
            .await
            .map_err(|error| SubscribeError::Discover(error.to_string()))?
            .map_err(SubscribeError::Discover)?;
        let client = Arc::new(
            Connector::default()
                .unix_domain_socket(socket)
                .connect()
                .await
                .map_err(SubscribeError::Connect)?,
        );
        *slot = Some(Arc::clone(&client));
        Ok(client)
    }

    /// Drops `failed` as the shared client, unless another task already
    /// replaced it.
    async fn forget(&self, failed: &Arc<Client>) {
        let mut slot = self.client.lock().await;
        if slot
            .as_ref()
            .is_some_and(|client| Arc::ptr_eq(client, failed))
        {
            *slot = None;
        }
    }
}

/// One workspace's subscription task.
struct Task {
    shared: Arc<Shared>,
    workspace: WorkspaceId,
    root: PathBuf,
    skip: Vec<PathBuf>,
    stop: CancellationToken,
}

impl Task {
    /// The state machine: subscribed, reconnecting(attempt), stopped.
    async fn run(self) {
        let ws = self.workspace;
        let mut established = false;
        let mut attempt: u32 = 0;
        loop {
            let subscribed = tokio::select! {
                () = self.stop.cancelled() => return,
                subscribed = self.subscribe() => subscribed,
            };
            match subscribed {
                Ok((client, mut subscription)) => {
                    if established {
                        tracing::info!(
                            workspace_id = %ws,
                            path = %self.root.display(),
                            "watchman subscription re-established"
                        );
                    } else {
                        tracing::info!(
                            workspace_id = %ws,
                            path = %self.root.display(),
                            "watchman subscription established"
                        );
                    }
                    established = true;
                    attempt = 0;
                    self.report(true);
                    match self.pump(&mut subscription).await {
                        Ended::Stopped => {
                            let cancel = subscription.cancel();
                            let _ = tokio::time::timeout(UNSUBSCRIBE_TIMEOUT, cancel).await;
                            return;
                        }
                        Ended::Lost {
                            reason,
                            lost_connection,
                        } => {
                            tracing::warn!(
                                workspace_id = %ws,
                                reason = %reason,
                                "watchman subscription lost; the workspace is snapshotted on the \
                                 fallback timer until it is back"
                            );
                            drop(subscription);
                            if lost_connection {
                                self.shared.forget(&client).await;
                            }
                        }
                    }
                }
                Err((client, error)) => {
                    // The first failure of a run is a warning; the retries
                    // that follow are not worth one each.
                    if attempt == 0 {
                        tracing::warn!(
                            workspace_id = %ws,
                            path = %self.root.display(),
                            error = %ErrorChain(&error),
                            "cannot subscribe to watchman; the workspace is snapshotted on the \
                             fallback timer until it can"
                        );
                    } else {
                        tracing::debug!(
                            workspace_id = %ws,
                            attempt,
                            error = %ErrorChain(&error),
                            "cannot subscribe to watchman"
                        );
                    }
                    if let Some(client) = client.filter(|_| error.lost_connection()) {
                        self.shared.forget(&client).await;
                    }
                }
            }
            self.report(false);
            let delay = backoff(attempt);
            attempt = attempt.saturating_add(1);
            tokio::select! {
                () = self.stop.cancelled() => return,
                () = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// Connects (or reuses the connection), resolves the root, and
    /// subscribes. The client is returned with an error after connecting,
    /// so a lost connection can be dropped.
    async fn subscribe(
        &self,
    ) -> Result<(Arc<Client>, Subscription<NameOnly>), (Option<Arc<Client>>, SubscribeError)> {
        let client = self.shared.client().await.map_err(|error| (None, error))?;
        let fail = |error| (Some(Arc::clone(&client)), error);
        let canonical = CanonicalPath::canonicalize(&self.root)
            .map_err(|source| fail(SubscribeError::Canonicalize(self.root.clone(), source)))?;
        let resolved = client
            .resolve_root(canonical)
            .await
            .map_err(|source| fail(SubscribeError::Resolve(self.root.clone(), source)))?;
        let request = SubscribeRequest {
            expression: exclude_expression(&self.root, &self.skip),
            // The first result, and one after Watchman restarts or recrawls,
            // would list every file; an empty one says the same.
            empty_on_fresh_instance: true,
            ..SubscribeRequest::default()
        };
        let (subscription, _) = client
            .subscribe::<NameOnly>(&resolved, request)
            .await
            .map_err(|source| fail(SubscribeError::Subscribe(self.root.clone(), source)))?;
        Ok((client, subscription))
    }

    /// Delivers the subscription's batches as debounced events until it
    /// ends. Every subscription starts with a fresh-instance result, and
    /// Watchman sends another after it restarts or recrawls; each asks for
    /// one snapshot, since changes made before (or while Watchman was not
    /// watching) were seen by nobody. jj's own clock decides how much that
    /// snapshot scans.
    async fn pump(&self, subscription: &mut Subscription<NameOnly>) -> Ended {
        let mut first = true;
        let mut pending: Option<Pending> = None;
        let mut deadline = tokio::time::Instant::now();
        loop {
            tokio::select! {
                () = self.stop.cancelled() => return Ended::Stopped,
                () = tokio::time::sleep_until(deadline), if pending.is_some() => {
                    if let Some(batch) = pending.take() {
                        self.send(batch);
                    }
                }
                next = subscription.next() => {
                    let result = match next {
                        Ok(SubscriptionData::FilesChanged(result)) => result,
                        Ok(SubscriptionData::StateEnter { .. } | SubscriptionData::StateLeave { .. }) => {
                            continue;
                        }
                        Ok(SubscriptionData::Canceled) => {
                            return Ended::Lost {
                                reason: "watchman cancelled the subscription".to_owned(),
                                lost_connection: false,
                            };
                        }
                        Err(error) => {
                            return Ended::Lost {
                                reason: ErrorChain(&error).to_string(),
                                lost_connection: is_connection_error(&error),
                            };
                        }
                    };
                    let initial = std::mem::replace(&mut first, false);
                    let new_batch = || Pending {
                        at: Instant::now(),
                        paths: Vec::new(),
                        rescan: false,
                    };
                    if result.is_fresh_instance {
                        if !initial {
                            tracing::info!(
                                workspace_id = %self.workspace,
                                "watchman restarted or recrawled the workspace; snapshotting it"
                            );
                        }
                        pending.get_or_insert_with(new_batch).rescan = true;
                    } else {
                        let paths: Vec<PathBuf> = result
                            .files
                            .unwrap_or_default()
                            .into_iter()
                            .map(|file| self.root.join(file.name.into_inner()))
                            .filter(|path| !self.skip.iter().any(|prefix| path.starts_with(prefix)))
                            .collect();
                        if paths.is_empty() {
                            // Everything was under a skipped prefix.
                            continue;
                        }
                        pending.get_or_insert_with(new_batch).paths.extend(paths);
                    }
                    deadline = tokio::time::Instant::now() + self.shared.debounce;
                }
            }
        }
    }

    fn send(&self, batch: Pending) {
        // An empty path list is the scheduler's "anything may have changed".
        let paths = if batch.rescan {
            Vec::new()
        } else {
            batch.paths
        };
        let sent = self.shared.events.try_send(WatchEvent {
            workspace: self.workspace,
            paths,
            at: batch.at,
        });
        if let Err(mpsc::error::TrySendError::Full(_)) = sent {
            // The fallback timer still snapshots the workspace.
            tracing::debug!(workspace_id = %self.workspace, "watch event dropped; the scheduler is behind");
        }
    }

    /// Tells the scheduler whether the subscription is up, unless the
    /// workspace was unwatched meanwhile.
    fn report(&self, healthy: bool) {
        if !self.stop.is_cancelled() {
            (self.shared.health)(self.workspace, healthy);
        }
    }
}

/// Changes waiting for the debounce to pass.
struct Pending {
    /// When the first arrived.
    at: Instant,
    paths: Vec<PathBuf>,
    /// Watchman restarted or recrawled: anything may have changed.
    rescan: bool,
}

/// The subscription's expression: everything but the skipped prefixes
/// (the `[snapshot] exclude` directories and mounts), so build output does
/// not wake the daemon. `None` with nothing to skip.
fn exclude_expression(root: &Path, skip: &[PathBuf]) -> Option<Expr> {
    let relative: Vec<PathBuf> = skip
        .iter()
        .filter_map(|prefix| prefix.strip_prefix(root).ok())
        .filter(|prefix| !prefix.as_os_str().is_empty())
        .map(Path::to_owned)
        .collect();
    if relative.is_empty() {
        return None;
    }
    let mut terms = vec![Expr::Name(NameTerm {
        paths: relative.clone(),
        wholename: true,
    })];
    terms.extend(
        relative
            .into_iter()
            .map(|path| Expr::DirName(DirNameTerm { path, depth: None })),
    );
    Some(Expr::Not(Box::new(Expr::Any(terms))))
}

/// The delay before reconnect attempt `attempt + 1`: 1 s, 2 s, 4 s, ...,
/// at most 30 s.
fn backoff(attempt: u32) -> Duration {
    BACKOFF_FIRST
        .saturating_mul(2u32.saturating_pow(attempt.min(16)))
        .min(BACKOFF_MAX)
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_to_thirty_seconds() {
        let delays: Vec<u64> = (0..8).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30, 30]);
        assert_eq!(backoff(u32::MAX), BACKOFF_MAX);
    }

    #[test]
    fn expression_excludes_skipped_prefixes() {
        assert!(exclude_expression(Path::new("/w"), &[]).is_none());
        assert!(
            exclude_expression(Path::new("/w"), &[PathBuf::from("/elsewhere/target")]).is_none(),
            "prefixes outside the root are dropped"
        );
        let Some(Expr::Not(inner)) = exclude_expression(
            Path::new("/w"),
            &[PathBuf::from("/w/target"), PathBuf::from("/w/node_modules")],
        ) else {
            panic!("expected a negation");
        };
        let Expr::Any(terms) = *inner else {
            panic!("expected anyof");
        };
        assert_eq!(terms.len(), 3, "one name term and a dirname per prefix");
        let Expr::Name(NameTerm { paths, wholename }) = &terms[0] else {
            panic!("expected a name term first");
        };
        assert!(*wholename);
        assert_eq!(
            paths,
            &[PathBuf::from("target"), PathBuf::from("node_modules")]
        );
    }
}
