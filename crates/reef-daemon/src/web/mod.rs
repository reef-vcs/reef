//! The web UI's loopback listener (R2.18, ADR 0047).
//!
//! The first `CreateWebSession` binds `127.0.0.1` and `[::1]` on one port
//! (`[ui] listen`'s) and serves the embedded app and the read side of the
//! API over gRPC-web as `http://localhost:<port>`, behind
//! `gate::WebGateLayer`. A daemon whose user never runs `reef ui` opens no
//! TCP port. Once bound, the listener stays until the daemon stops: a tab's
//! origin is its port.
//!
//! A policy's `[passkeys]` pins that origin (#346, ADR 0094), so the port
//! does not change between runs of a daemon in the default state directory,
//! and once a passkey is registered the daemon holds it from its start
//! (`WebUi::hold`): no other process serves the pinned origin while the
//! daemon runs. A port another process holds is an error and never
//! replaced by another, which would teach the person that it varies.
//!
//! Sessions (`session`) are bearer tokens minted by `CreateWebSession`
//! over the socket, kept as digests, never persisted, so a daemon restart
//! ends them all. Launch files (`launch`) hand a token to a browser
//! without a command line.
//!
//! The listener serves READ methods and a short allowlist of writes
//! (`rules`, R3.43): passkeys, approvals, and evidence a passkey approved.
//! A request that passes carries a [`Caller`] naming its session.

pub(crate) mod assets;
pub(crate) mod body;
pub(crate) mod clear;
pub(crate) mod gate;
mod launch;
pub(crate) mod rules;
pub(crate) mod session;

use std::io;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use futures::Stream;
use futures::stream::SelectAll;
use nix::errno::Errno;
use rand::RngExt as _;
use reef_api::v1::{CreateWebSessionResponse, WebUiState, WebUiStatus};
use reef_lib::error::ErrorChain;
use reef_lib::error::{Classify, ErrorKind};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tonic::transport::server::{Connected, TcpConnectInfo};

use crate::daemon::{AppState, SHUTDOWN_GRACE};
use crate::error::{Advice, Advise};
use crate::limits::Accepting;
use session::Sessions;

/// The web listener's limits. Not configuration-file keys: tests shrink
/// them through `Config::web`.
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    /// Live sessions at most; minting one more ends the least recently
    /// used.
    pub max_sessions: usize,
    /// A session unused this long, with no stream open, ends.
    pub session_idle: Duration,
    /// A launch file whose session never made a request is deleted after
    /// this long.
    pub launch_file_ttl: Duration,
    /// Open connections at most; one more is accepted and closed at once.
    pub max_connections: usize,
    /// Whether the embedded assets are the app. Defaults to
    /// [`reef_web::BUILT`]; `CreateWebSession` refuses a placeholder build
    /// with `WEB_UI_NOT_BUILT`. Test daemons set it, so they serve the
    /// placeholder through the full gate.
    pub assets_built: bool,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_sessions: 32,
            session_idle: Duration::from_hours(24),
            launch_file_ttl: Duration::from_secs(60),
            max_connections: 64,
            assets_built: reef_web::BUILT,
        }
    }
}

/// Failure of `CreateWebSession`, and the web listener's refusals.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum WebError {
    /// `[ui] enabled = false`.
    #[error("the web UI is disabled")]
    Disabled,
    /// The daemon embeds the placeholder, not the app.
    #[error("this reef-daemon was built without the web UI")]
    NotBuilt,
    /// The listener could not be bound.
    #[error("cannot listen on {address} for the web UI")]
    Unavailable {
        /// The address that could not be bound, `127.0.0.1:<port>` or
        /// `[::1]:<port>`, or `[ui] listen` when it is not loopback.
        address: String,
        /// The bind error.
        #[source]
        source: io::Error,
    },
    /// The launch file could not be written.
    #[error("cannot write the web UI's launch file in {}", dir.display())]
    LaunchFile {
        /// `<state>/ui`.
        dir: PathBuf,
        /// The error.
        #[source]
        source: io::Error,
    },
    /// A web listener request without a live session.
    #[error("no live web UI session")]
    SessionInvalid,
    /// A web listener request for a method that is neither `ACCESS_READ`
    /// nor on the allowlist (`rules::TABLE`).
    #[error("the web UI serves read RPCs and its allowlist of writes; {method} is {access}")]
    AccessDenied {
        /// The method's path.
        method: String,
        /// `write`, `admin` or `unknown`.
        access: &'static str,
    },
    /// A web listener write its allowlist row's rule refuses (R3.31).
    #[error("through the web UI, {method} is allowed only by its rule {rule}")]
    RuleDenied {
        /// The method's path.
        method: String,
        /// The rule, by its name in `web/rules.rs`.
        rule: &'static str,
    },
    /// A `CreateWebSession` path that is not one (R3.43).
    #[error("{path:?} is not a path of the web UI")]
    InvalidPath {
        /// The path.
        path: String,
    },
}

impl Classify for WebError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Disabled | Self::NotBuilt => ErrorKind::Conflict,
            Self::Unavailable { .. } => ErrorKind::Unavailable,
            Self::LaunchFile { .. } => ErrorKind::Internal,
            Self::SessionInvalid | Self::AccessDenied { .. } | Self::RuleDenied { .. } => {
                ErrorKind::PermissionDenied
            }
            Self::InvalidPath { .. } => ErrorKind::InvalidArgument,
        }
    }
}

impl Advise for WebError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Disabled => Some(Advice::hint(
                "The web UI is off: set `[ui] enabled = true` in the daemon's config.toml and restart the daemon.",
            )),
            Self::NotBuilt => Some(Advice::hint(
                "This reef-daemon was built without the web UI. Install a release (https://github.com/reef-vcs/reef/releases, `install.sh`, Homebrew), or run `just web-build` before `cargo build` in a checkout, then restart the daemon.",
            )),
            Self::Unavailable { address, .. } => Some(Advice::hint(format!(
                "Another process holds `{address}`: `lsof -nP -iTCP:{} -sTCP:LISTEN` names it. If it is not a daemon of yours, do not touch a passkey on any page at that address. Stop it, run `reef ui` again and, since it may have left a page of its own in your browser, clear the browser's data for that address. Or move Reef: set another port in `[ui] listen`, restart the daemon, and pin the new address in `[passkeys]`.",
                address.rsplit(':').next().unwrap_or_default()
            ))),
            Self::SessionInvalid => Some(Advice::run(
                "This page's session ended (the daemon restarted, or it went unused for a day). Run `reef ui` in a terminal.",
                ["reef", "ui"],
            )),
            Self::AccessDenied { .. } => Some(Advice::hint(
                "The web UI serves read RPCs and an allowlist of writes (passkeys, approvals, evidence a passkey approved, declines, comments and landings); use the CLI for this.",
            )),
            Self::RuleDenied { rule, .. } => Some(Advice::hint(match *rule {
                "decline" => {
                    "Through the web UI a transition only declines, by a person, at the commit the page showed (`expected_commit_id`), naming no workspace; use the CLI for anything else."
                }
                "comment" => {
                    "Through the web UI a comment is a person's note, instruction or answer: no help request, no confirm in the same call (confirm after its estimate), and no workspace or client session; use the CLI for anything else."
                }
                "confirm" => {
                    "Through the web UI an instruction is only confirmed or cancelled; the session that claimed it releases, starts or finishes it."
                }
                "land" => {
                    "Through the web UI a landing is asked for by a person, at the commit the page showed (`expected_commit_id`), naming no workspace; use the CLI for anything else."
                }
                _ => {
                    "The web UI allows this write only under its rule in web/rules.rs; use the CLI."
                }
            })),
            Self::InvalidPath { .. } => Some(Advice::hint(
                "Give a path of the web UI starting with /, of letters, digits and /._-, such as /approve/apr_0123456789abcdef, or none.",
            )),
            Self::LaunchFile { .. } => None,
        }
    }
}

/// The bound listener.
#[derive(Debug)]
struct Bound {
    /// `http://<authority>/`.
    url: String,
}

/// Who called through the web listener (R3.43): the gate puts it in every
/// request it passes, where a client cannot set it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Caller {
    /// The session: the first 16 hex digits of its token's digest.
    pub session: String,
}

/// The web UI: its sessions, and its listener once bound.
#[derive(Debug)]
pub struct WebUi {
    bound: tokio::sync::Mutex<Option<Bound>>,
    sessions: Arc<Sessions>,
    /// `http://localhost:<port>`, once bound: the one origin a passkey
    /// ceremony is accepted from.
    origin: OnceLock<String>,
    /// Why the last bind failed, until one succeeds.
    problem: Mutex<Option<String>>,
    assets: Arc<assets::Assets>,
    resets: Arc<clear::Resets>,
}

/// How long [`WebUi::hold`] waits after its first failed bind, doubled after
/// each up to [`HOLD_RETRY_MAX`].
const HOLD_RETRY_FIRST: Duration = Duration::from_millis(250);

/// The longest [`WebUi::hold`] waits between binds.
const HOLD_RETRY_MAX: Duration = Duration::from_secs(5);

impl WebUi {
    pub(crate) fn new(limits: Limits) -> WebUi {
        WebUi {
            bound: tokio::sync::Mutex::new(None),
            sessions: Arc::new(Sessions::new(limits)),
            origin: OnceLock::new(),
            problem: Mutex::new(None),
            assets: Arc::new(assets::Assets::new()),
            resets: Arc::default(),
        }
    }

    /// This run's assets.
    pub(crate) fn assets(&self) -> Arc<assets::Assets> {
        Arc::clone(&self.assets)
    }

    /// The bound listener's URL, binding it if needed.
    async fn ensure_bound(state: &Arc<AppState>) -> Result<String, WebError> {
        let web = &state.web;
        let mut bound = web.bound.lock().await;
        if let Some(bound) = &*bound {
            return Ok(bound.url.clone());
        }
        let attempt = bind(state).await;
        *web.problem.lock().unwrap_or_else(PoisonError::into_inner) = attempt
            .as_ref()
            .err()
            .map(|error| ErrorChain(error).to_string());
        Ok(bound.insert(attempt?).url.clone())
    }

    /// Binds the listener at the daemon's start, when its port is fixed and
    /// a passkey is registered (#346): a policy may pin the origin, so no
    /// other process may serve it while the daemon runs. A failure is logged
    /// once, shows in the daemon's status, and is tried again until the
    /// port is the daemon's, so a process that lets it go leaves it to no
    /// one else.
    pub(crate) async fn hold(state: &Arc<AppState>) {
        let ui = &state.config.file.ui;
        if !ui.enabled || !state.config.web.assets_built || !ui.listen_port().is_ok_and(|p| p != 0)
        {
            return;
        }
        let mut wait = HOLD_RETRY_FIRST;
        loop {
            let failed = match state.db.passkeys(None, false) {
                Ok(passkeys) if passkeys.is_empty() => return,
                Ok(_) => match WebUi::ensure_bound(state).await {
                    Ok(_) => return,
                    Err(error) => ErrorChain(&error).to_string(),
                },
                Err(error) => ErrorChain(&error).to_string(),
            };
            if wait == HOLD_RETRY_FIRST {
                tracing::warn!(
                    error = %failed,
                    "the web UI's port is not this daemon's: do not touch a passkey on a page at its address; trying until it is"
                );
            }
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = state.shutdown.cancelled() => return,
            }
            wait = (wait * 2).min(HOLD_RETRY_MAX);
        }
    }

    /// The listener's state, as `GetDaemonStatus` reports it.
    pub(crate) fn status(state: &AppState) -> WebUiStatus {
        let ui = &state.config.file.ui;
        let port = ui.listen_port().unwrap_or_default();
        let web = &state.web;
        let problem = web
            .problem
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let listener = if !ui.enabled {
            WebUiState::Disabled
        } else if !state.config.web.assets_built {
            WebUiState::NotBuilt
        } else if web.origin().is_some() {
            WebUiState::Bound
        } else if problem.is_some() {
            WebUiState::Unavailable
        } else {
            WebUiState::Unbound
        };
        let origin = match web.origin() {
            Some(origin) => origin.to_owned(),
            None if port != 0 => format!("http://localhost:{port}"),
            None => String::new(),
        };
        WebUiStatus {
            state: listener.into(),
            origin,
            fixed: port != 0,
            problem: problem.unwrap_or_default(),
        }
    }

    /// `http://localhost:<port>` once the listener is bound.
    pub fn origin(&self) -> Option<&str> {
        self.origin.get().map(String::as_str)
    }

    /// `CreateWebSession`: binds the listener if needed, mints a session,
    /// and writes its launch file if asked, opening the app at `path`.
    pub(crate) async fn create_session(
        state: &Arc<AppState>,
        launch_file: bool,
        path: &str,
    ) -> Result<CreateWebSessionResponse, WebError> {
        if !reef_client::web::is_app_path(path) {
            return Err(WebError::InvalidPath {
                path: path.to_owned(),
            });
        }
        let ui = &state.config.file.ui;
        if !ui.enabled {
            return Err(WebError::Disabled);
        }
        if !state.config.web.assets_built {
            return Err(WebError::NotBuilt);
        }
        let web = &state.web;
        let url = WebUi::ensure_bound(state).await?;
        let launch_dir = &state.config.paths.ui;
        let (token, ended) = web.sessions.mint();
        for path in ended {
            launch::remove(&path);
        }
        let page = path.strip_prefix('/').unwrap_or_default();
        // A URL few caches hold: at a port that does not change, another
        // process that held it may have had the browser cache its own page.
        // Chromium's cache can be told to ignore the query, so a launch file
        // has it cleared first (`clear`).
        let launch = rand::rng().random::<u64>();
        let session_url = format!("{url}{page}?launch={launch:016x}#session={token}");
        let launch_path = if launch_file {
            let dir = launch_dir.clone();
            let opens = launch::Opens {
                session_url: session_url.clone(),
                reset: ui
                    .listen_port()
                    .is_ok_and(|port| port != 0)
                    .then(|| launch::Reset {
                        origin: url.trim_end_matches('/').to_owned(),
                        id: web.resets.mint(),
                    }),
            };
            let path = tokio::task::spawn_blocking(move || launch::write(&dir, &opens))
                .await
                .map_err(io::Error::other)
                .and_then(|written| written)
                .map_err(|source| WebError::LaunchFile {
                    dir: launch_dir.clone(),
                    source,
                })?;
            web.sessions.attach_launch_file(&token, path.clone());
            expire_launch_file(state, path.clone());
            path.display().to_string()
        } else {
            String::new()
        };
        Ok(CreateWebSessionResponse {
            url,
            session_url,
            launch_path,
        })
    }

    /// Whether the listener is bound: false until the first session.
    pub fn is_bound(&self) -> bool {
        self.bound.try_lock().is_ok_and(|bound| bound.is_some())
    }

    /// The start-time sweep of launch files an earlier run left in `dir`.
    pub(crate) fn sweep(dir: &std::path::Path) {
        let removed = launch::sweep(dir);
        if removed > 0 {
            tracing::info!(removed_count = removed, "swept web UI launch files");
        }
    }
}

/// Deletes `path` after the launch file time to live, unless its session's
/// first request deleted it first.
fn expire_launch_file(state: &Arc<AppState>, path: PathBuf) {
    let ttl = state.config.web.launch_file_ttl;
    let weak = Arc::downgrade(state);
    let shutdown = state.shutdown.clone();
    tokio::spawn(async move {
        tokio::select! {
            () = tokio::time::sleep(ttl) => {}
            () = shutdown.cancelled() => {}
        }
        launch::remove(&path);
        if let Some(state) = weak.upgrade() {
            state.web.sessions.launch_file_gone(&path);
        }
    });
}

/// Binds both loopback families on `[ui] listen`'s port and starts serving
/// on them; the server stops with the daemon, joined by a shutdown hook.
async fn bind(state: &Arc<AppState>) -> Result<Bound, WebError> {
    let ui = &state.config.file.ui;
    // Validated at load; an address `localhost` does not reach never binds.
    let port = ui.listen_port().map_err(|reason| WebError::Unavailable {
        address: ui.listen.clone(),
        source: io::Error::other(reason),
    })?;
    if port != 0 {
        refuse_answering(port).await?;
    }
    let loopback = Loopback::bind(port).await?;
    let authority = format!("localhost:{}", loopback.port);
    let origin = format!("http://{authority}");
    let _ = state.web.origin.set(origin.clone());
    let gate = Arc::new(gate::Gate {
        origin,
        authority: authority.clone(),
        sessions: Arc::clone(&state.web.sessions),
        assets: state.web.assets(),
        resets: Arc::clone(&state.web.resets),
    });
    let ipv6 = loopback.v6.is_some();
    let accepting: SelectAll<_> = std::iter::once(loopback.v4)
        .chain(loopback.v6)
        .map(Accepting::new)
        .collect();
    let incoming = Capped::new(accepting, state.config.web.max_connections);
    let server = tokio::spawn(
        crate::server::web_router(state, gate)
            .serve_with_incoming_shutdown(incoming, state.shutdown.clone().cancelled_owned()),
    );
    state.hooks.add("web listener", move || {
        Box::pin(async move {
            let mut server = server;
            match tokio::time::timeout(SHUTDOWN_GRACE, &mut server).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(error))) => tracing::warn!(%error, "the web listener failed"),
                Ok(Err(error)) => tracing::warn!(%error, "the web listener panicked"),
                Err(_) => server.abort(),
            }
        })
    });
    tracing::info!(address = %authority, ipv6, "web UI listening");
    Ok(Bound {
        url: format!("http://{authority}/"),
    })
}

/// How long [`refuse_answering`] waits for an answer on each family.
const PROBE: Duration = Duration::from_millis(250);

/// Refuses a fixed port another process already answers on. The bind alone
/// does not see one that listens on every address: on macOS both binds
/// succeed under it, and it serves the origin once the daemon stops.
async fn refuse_answering(port: u16) -> Result<(), WebError> {
    for addr in [
        SocketAddr::from((Ipv4Addr::LOCALHOST, port)),
        SocketAddr::from((Ipv6Addr::LOCALHOST, port)),
    ] {
        if let Ok(Ok(stream)) = tokio::time::timeout(PROBE, TcpStream::connect(addr)).await
            // A connect from the port to itself, when the kernel picks it as
            // the source: nobody listens.
            && stream.local_addr().ok() != stream.peer_addr().ok()
        {
            return Err(WebError::Unavailable {
                address: addr.to_string(),
                source: io::Error::new(io::ErrorKind::AddrInUse, "another process answers there"),
            });
        }
    }
    Ok(())
}

/// How many kernel-chosen ports [`Loopback::bind`] tries, each free on
/// `127.0.0.1` but taken on `[::1]`, before it gives up.
const PORT_ATTEMPTS: usize = 16;

/// The listener's sockets: both loopback families on one port, so that
/// whichever `localhost` resolves to first is the daemon. A process holding
/// the other family's address at that port would serve the daemon's exact
/// origin (S-251). Never `SO_REUSEPORT`: without it, no other user's socket
/// can bind either address while the daemon listens.
#[derive(Debug)]
struct Loopback {
    port: u16,
    v4: TcpListener,
    /// `None` only on a host with no IPv6 loopback, where nobody can bind
    /// `[::1]` either.
    v6: Option<TcpListener>,
}

impl Loopback {
    /// Binds `127.0.0.1:<port>` and `[::1]:<port>`. With port 0, it takes
    /// the port the kernel picks for IPv4, and picks again while that port
    /// is taken on `[::1]`; with a fixed port, either address taken is an
    /// error naming it.
    async fn bind(port: u16) -> Result<Loopback, WebError> {
        let unavailable = |addr: SocketAddr, source| WebError::Unavailable {
            address: addr.to_string(),
            source,
        };
        // Ports already tried stay held, so the kernel never offers one
        // twice.
        let mut tried = Vec::new();
        loop {
            let v4_addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
            let v4 = TcpListener::bind(v4_addr)
                .await
                .map_err(|source| unavailable(v4_addr, source))?;
            let port = v4
                .local_addr()
                .map_err(|source| unavailable(v4_addr, source))?
                .port();
            let v6_addr = SocketAddr::from((Ipv6Addr::LOCALHOST, port));
            let v6 = match TcpListener::bind(v6_addr).await {
                Ok(v6) => Some(v6),
                Err(error) if no_ipv6_loopback(&error) => {
                    tracing::info!(%error, "no IPv6 loopback; the web UI listens on 127.0.0.1 only");
                    None
                }
                Err(error) if v4_addr.port() == 0 && error.kind() == io::ErrorKind::AddrInUse => {
                    tried.push(v4);
                    if tried.len() < PORT_ATTEMPTS {
                        continue;
                    }
                    return Err(unavailable(
                        v6_addr,
                        io::Error::new(
                            error.kind(),
                            format!(
                                "{error}, as was [::1] at each of the {PORT_ATTEMPTS} ports the kernel picked for 127.0.0.1"
                            ),
                        ),
                    ));
                }
                Err(error) => return Err(unavailable(v6_addr, error)),
            };
            return Ok(Loopback { port, v4, v6 });
        }
    }
}

/// Whether binding `[::1]` failed because the host has no IPv6 loopback:
/// the address is not configured, or IPv6 is off.
fn no_ipv6_loopback(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::AddrNotAvailable
        || error.raw_os_error() == Some(Errno::EAFNOSUPPORT as i32)
}

/// A TCP connection counted against [`Limits::max_connections`] until it
/// drops.
#[derive(Debug)]
pub(crate) struct Counted {
    stream: TcpStream,
    open: Arc<AtomicUsize>,
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.open.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Connected for Counted {
    type ConnectInfo = TcpConnectInfo;

    fn connect_info(&self) -> TcpConnectInfo {
        self.stream.connect_info()
    }
}

impl AsyncRead for Counted {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Counted {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.stream.is_write_vectored()
    }
}

/// The listener's connections, from both families, at most `max` open at
/// once: one more is accepted and closed, with a warning at most every 10 s.
#[derive(Debug)]
struct Capped {
    inner: SelectAll<Accepting<TcpListener>>,
    open: Arc<AtomicUsize>,
    max: usize,
    warned_at: Option<Instant>,
}

impl Capped {
    fn new(inner: SelectAll<Accepting<TcpListener>>, max: usize) -> Capped {
        Capped {
            inner,
            open: Arc::new(AtomicUsize::new(0)),
            max,
            warned_at: None,
        }
    }
}

impl Stream for Capped {
    type Item = io::Result<Counted>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let stream = match ready!(Pin::new(&mut self.inner).poll_next(cx)) {
                Some(Ok(stream)) => stream,
                Some(Err(error)) => return Poll::Ready(Some(Err(error))),
                None => return Poll::Ready(None),
            };
            if self.open.load(Ordering::Relaxed) >= self.max {
                drop(stream);
                if self
                    .warned_at
                    .is_none_or(|at| at.elapsed() >= Duration::from_secs(10))
                {
                    tracing::warn!(
                        limit = self.max,
                        "the web listener is at its connection limit; closing a new connection"
                    );
                    self.warned_at = Some(Instant::now());
                }
                continue;
            }
            self.open.fetch_add(1, Ordering::Relaxed);
            return Poll::Ready(Some(Ok(Counted {
                stream,
                open: Arc::clone(&self.open),
            })));
        }
    }
}
