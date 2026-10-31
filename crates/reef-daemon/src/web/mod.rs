//! The web UI's loopback listener (R2.18, ADR 0047).
//!
//! The first `CreateWebSession` binds `[ui] listen` (loopback only, a
//! kernel-chosen port by default) and serves the embedded app and the read
//! side of the API over gRPC-web, behind `gate::WebGateLayer`. A daemon
//! whose user never runs `reef ui` opens no TCP port. Once bound, the
//! listener stays until the daemon stops: a tab's origin is its port.
//!
//! Sessions (`session`) are bearer tokens minted by `CreateWebSession`
//! over the socket, kept as digests, never persisted, so a daemon restart
//! ends them all. Launch files (`launch`) hand a token to a browser
//! without a command line.

mod assets;
mod body;
pub(crate) mod gate;
mod launch;
pub(crate) mod session;

use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use futures::Stream;
use reef_api::v1::CreateWebSessionResponse;
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
        /// `[ui] listen`.
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
    /// A web listener request for a method that is not `ACCESS_READ`.
    #[error("the web UI serves read RPCs only; {method} is {access}")]
    AccessDenied {
        /// The method's path.
        method: String,
        /// `write`, `admin` or `unknown`.
        access: &'static str,
    },
}

impl Classify for WebError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Disabled | Self::NotBuilt => ErrorKind::Conflict,
            Self::Unavailable { .. } => ErrorKind::Unavailable,
            Self::LaunchFile { .. } => ErrorKind::Internal,
            Self::SessionInvalid | Self::AccessDenied { .. } => ErrorKind::PermissionDenied,
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
                "Another process holds `{address}`. Free it, or set `[ui] listen = \"127.0.0.1:0\"` for a free port, restart the daemon, and run `reef ui` again."
            ))),
            Self::SessionInvalid => Some(Advice::run(
                "This page's session ended (the daemon restarted, or it went unused for a day). Run `reef ui` in a terminal.",
                ["reef", "ui"],
            )),
            Self::AccessDenied { .. } => Some(Advice::hint(
                "The web UI serves read RPCs only; use the CLI for this.",
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

/// The web UI: its sessions, and its listener once bound.
#[derive(Debug)]
pub struct WebUi {
    bound: tokio::sync::Mutex<Option<Bound>>,
    sessions: Arc<Sessions>,
}

impl WebUi {
    pub(crate) fn new(limits: Limits) -> WebUi {
        WebUi {
            bound: tokio::sync::Mutex::new(None),
            sessions: Arc::new(Sessions::new(limits)),
        }
    }

    /// `CreateWebSession`: binds the listener if needed, mints a session,
    /// and writes its launch file if asked.
    pub(crate) async fn create_session(
        state: &Arc<AppState>,
        launch_file: bool,
    ) -> Result<CreateWebSessionResponse, WebError> {
        let ui = &state.config.file.ui;
        if !ui.enabled {
            return Err(WebError::Disabled);
        }
        if !state.config.web.assets_built {
            return Err(WebError::NotBuilt);
        }
        let web = &state.web;
        let url = {
            let mut bound = web.bound.lock().await;
            if bound.is_none() {
                *bound = Some(bind(state).await?);
            }
            bound.as_ref().map(|b| b.url.clone()).unwrap_or_default()
        };
        let launch_dir = &state.config.paths.ui;
        let (token, ended) = web.sessions.mint();
        for path in ended {
            launch::remove(&path);
        }
        let session_url = format!("{url}#session={token}");
        let launch_path = if launch_file {
            let dir = launch_dir.clone();
            let page = session_url.clone();
            let path = tokio::task::spawn_blocking(move || launch::write(&dir, &page))
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

/// Binds `[ui] listen` and starts serving on it; the server stops with the
/// daemon, joined by a shutdown hook.
async fn bind(state: &Arc<AppState>) -> Result<Bound, WebError> {
    let ui = &state.config.file.ui;
    let unavailable = |source| WebError::Unavailable {
        address: ui.listen.clone(),
        source,
    };
    // Validated at load; a listen address that is not loopback never binds.
    let addr = ui
        .listen_addr()
        .ok_or_else(|| unavailable(io::Error::other("not a loopback address")))?;
    // Never SO_REUSEPORT: without it, no other user's socket can bind this
    // address and port while the daemon listens.
    let listener = TcpListener::bind(addr).await.map_err(unavailable)?;
    let local = listener.local_addr().map_err(unavailable)?;
    let authority = authority_of(local);
    let gate = Arc::new(gate::Gate {
        origin: format!("http://{authority}"),
        authority: authority.clone(),
        sessions: Arc::clone(&state.web.sessions),
    });
    let incoming = Capped::new(Accepting::new(listener), state.config.web.max_connections);
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
    tracing::info!(address = %authority, "web UI listening");
    Ok(Bound {
        url: format!("http://{authority}/"),
    })
}

/// `addr` as a URL authority: `127.0.0.1:<port>`, `[::1]:<port>`.
fn authority_of(addr: SocketAddr) -> String {
    addr.to_string()
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

/// The listener's connections, at most `max` open at once: one more is
/// accepted and closed, with a warning at most every 10 s.
#[derive(Debug)]
struct Capped {
    inner: Accepting<TcpListener>,
    open: Arc<AtomicUsize>,
    max: usize,
    warned_at: Option<Instant>,
}

impl Capped {
    fn new(inner: Accepting<TcpListener>, max: usize) -> Capped {
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
