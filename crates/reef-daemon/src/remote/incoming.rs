//! The remote listener's connections (R2.08): accepted, screened, and
//! handshaken off the accept loop.
//!
//! The accept loop closes a connection at once from an address in backoff,
//! or over the listener's or the address's connection limit, before any
//! TLS. Each other connection is handshaken in its own task, at most
//! `max_handshakes` at once (accepts wait for a slot; the kernel's backlog
//! holds them), each within `handshake_timeout`, and only finished
//! connections reach the server. A peer that connects and sends nothing
//! holds one slot for the timeout and no more.

use std::future::Future as _;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use futures::StreamExt as _;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;
use tokio_util::sync::CancellationToken;
use tonic::transport::server::Connected;

use super::Limits;
use super::failures::{Failures, Open};
use crate::limits::Accepting;

/// What the gate learns about a connection: tonic puts it in every
/// request's extensions.
#[derive(Clone, Debug)]
pub struct Peer {
    /// The client's address.
    pub addr: SocketAddr,
    /// Set by the gate once a request on the connection presented a live
    /// token.
    authenticated: Arc<AtomicBool>,
}

impl Peer {
    /// Marks the connection as having made an authenticated request, so
    /// `auth_timeout` no longer closes it.
    pub(crate) fn authenticated(&self) {
        self.authenticated.store(true, Ordering::Relaxed);
    }
}

/// A connection that finished its handshake, counted until it drops. One
/// that has made no authenticated request within `auth_timeout` of its
/// handshake is closed: without it, an address with no token could hold
/// connection slots for as long as it liked.
#[derive(Debug)]
pub(crate) struct Conn {
    stream: TlsStream<TcpStream>,
    peer: SocketAddr,
    authenticated: Arc<AtomicBool>,
    /// `None` once authenticated.
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
    _open: Open,
    _counted: Counted,
}

#[derive(Debug)]
struct Counted(Arc<AtomicUsize>);

impl Drop for Counted {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Connected for Conn {
    type ConnectInfo = Peer;

    fn connect_info(&self) -> Peer {
        Peer {
            addr: self.peer,
            authenticated: Arc::clone(&self.authenticated),
        }
    }
}

impl Conn {
    /// Whether the connection outlived `auth_timeout` without an
    /// authenticated request; registers the deadline's waker otherwise.
    fn expired(&mut self, cx: &mut Context<'_>) -> bool {
        if self.authenticated.load(Ordering::Relaxed) {
            self.deadline = None;
        }
        self.deadline
            .as_mut()
            .is_some_and(|deadline| deadline.as_mut().poll(cx).is_ready())
    }
}

impl AsyncRead for Conn {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.expired(cx) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no authenticated request within the remote listener's auth timeout",
            )));
        }
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}

impl AsyncWrite for Conn {
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

/// Accepts from `listener` until `shutdown`, sending handshaken connections
/// to the returned stream.
pub(crate) fn incoming(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    failures: Arc<Failures>,
    limits: &Limits,
    shutdown: CancellationToken,
) -> tokio_stream::wrappers::ReceiverStream<io::Result<Conn>> {
    let (tx, rx) = mpsc::channel(limits.max_handshakes.max(1));
    tokio::spawn(accept_loop(
        listener,
        acceptor,
        failures,
        Box::new(*limits),
        shutdown,
        tx,
    ));
    tokio_stream::wrappers::ReceiverStream::new(rx)
}

async fn accept_loop(
    listener: TcpListener,
    acceptor: TlsAcceptor,
    failures: Arc<Failures>,
    limits: Box<Limits>,
    shutdown: CancellationToken,
    tx: mpsc::Sender<io::Result<Conn>>,
) {
    let mut accepting = Accepting::new(listener);
    let handshakes = Arc::new(Semaphore::new(limits.max_handshakes.max(1)));
    let open = Arc::new(AtomicUsize::new(0));
    let mut warned_at: Option<Instant> = None;
    loop {
        let accepted = tokio::select! {
            () = shutdown.cancelled() => return,
            accepted = accepting.next() => accepted,
        };
        let stream = match accepted {
            Some(Ok(stream)) => stream,
            Some(Err(_)) => continue,
            None => return,
        };
        let Ok(peer) = stream.peer_addr() else {
            continue;
        };
        if failures.blocked(peer.ip()) {
            continue;
        }
        let over = open.load(Ordering::Relaxed) >= limits.max_connections;
        let slot = if over { None } else { failures.open(peer.ip()) };
        let Some(slot) = slot else {
            if warned_at.is_none_or(|at| at.elapsed() >= Duration::from_secs(10)) {
                tracing::warn!(
                    peer = %peer,
                    limit = if over { limits.max_connections } else { limits.max_connections_per_address },
                    "the remote listener is at a connection limit; closing a new connection"
                );
                warned_at = Some(Instant::now());
            }
            continue;
        };
        open.fetch_add(1, Ordering::Relaxed);
        let counted = Counted(Arc::clone(&open));
        let permit = tokio::select! {
            () = shutdown.cancelled() => return,
            permit = Arc::clone(&handshakes).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return,
            },
        };
        let (acceptor, tx) = (acceptor.clone(), tx.clone());
        let timeout = limits.handshake_timeout;
        let auth_timeout = limits.auth_timeout;
        tokio::spawn(async move {
            let handshake = tokio::time::timeout(timeout, acceptor.accept(stream)).await;
            drop(permit);
            match handshake {
                Ok(Ok(stream)) => {
                    let conn = Conn {
                        stream,
                        peer,
                        authenticated: Arc::new(AtomicBool::new(false)),
                        deadline: Some(Box::pin(tokio::time::sleep(auth_timeout))),
                        _open: slot,
                        _counted: counted,
                    };
                    let _ = tx.send(Ok(conn)).await;
                }
                Ok(Err(error)) => {
                    tracing::debug!(peer = %peer, %error, "remote TLS handshake failed");
                }
                Err(_) => {
                    tracing::debug!(peer = %peer, "remote TLS handshake timed out");
                }
            }
        });
    }
}
