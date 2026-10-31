//! What the daemon does about its descriptor limit (R2.22): it raises its
//! open-file limit, so that hundreds of clients can connect to one daemon
//! started from a shell whose default soft limit is 256 (macOS, `launchctl
//! limit maxfiles`) or 1,024 (common on Linux), and `Accepting` backs off
//! when the process runs out anyway, instead of spinning in tonic's accept
//! loop, which retries a failed `accept` at once and logs it only at
//! `trace`.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll, ready};
use std::time::{Duration, Instant};

use futures::Stream;
use nix::errno::Errno;
use nix::sys::resource::{Resource, getrlimit, setrlimit};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};

/// The soft open-file limit [`raise_open_file_limit`] asks for, within the
/// hard limit. macOS refuses a soft limit above `OPEN_MAX`.
#[cfg(target_os = "macos")]
pub const OPEN_FILES_WANTED: u64 = 10_240;
/// The soft open-file limit [`raise_open_file_limit`] asks for, within the
/// hard limit.
#[cfg(not(target_os = "macos"))]
pub const OPEN_FILES_WANTED: u64 = 65_536;

/// How long [`Accepting`] waits before accepting again after running out.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// [`Accepting`] warns at most this often.
const WARN_EVERY: Duration = Duration::from_secs(10);

/// Raises this process's soft `RLIMIT_NOFILE` to [`OPEN_FILES_WANTED`],
/// or to the hard limit if that is lower. Never lowers it. Returns the
/// soft limit before and after.
///
/// # Errors
///
/// The `getrlimit` or `setrlimit` error.
pub fn raise_open_file_limit() -> Result<(u64, u64), Errno> {
    let (soft, hard) = getrlimit(Resource::RLIMIT_NOFILE)?;
    let wanted = OPEN_FILES_WANTED.min(hard);
    if soft >= wanted {
        return Ok((soft, soft));
    }
    setrlimit(Resource::RLIMIT_NOFILE, wanted, hard)?;
    Ok((soft, wanted))
}

/// A listener [`Accepting`] accepts from: the socket, or the web UI's TCP
/// listener (R2.18).
pub(crate) trait Listen {
    /// An accepted connection.
    type Stream;

    /// Polls for the next connection.
    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<Self::Stream>>;
}

impl Listen for UnixListener {
    type Stream = UnixStream;

    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<UnixStream>> {
        UnixListener::poll_accept(self, cx).map_ok(|(stream, _)| stream)
    }
}

impl Listen for TcpListener {
    type Stream = TcpStream;

    fn poll_accept(&self, cx: &mut Context<'_>) -> Poll<io::Result<TcpStream>> {
        // Small streaming frames go out at once rather than waiting on
        // Nagle and a delayed ACK, as tonic's own `TcpIncoming` does.
        TcpListener::poll_accept(self, cx).map_ok(|(stream, _)| {
            stream.set_nodelay(true).ok();
            stream
        })
    }
}

/// A listener as the server's incoming connections. When
/// `accept` fails for want of descriptors or memory, it waits
/// [`ACCEPT_BACKOFF`] before accepting again and warns at most every
/// [`WARN_EVERY`]; other errors pass through, which the server skips.
#[derive(Debug)]
pub(crate) struct Accepting<L> {
    listener: L,
    backoff: Option<Pin<Box<tokio::time::Sleep>>>,
    /// Failures since the last warning.
    failures: u64,
    warned_at: Option<Instant>,
}

impl<L> Accepting<L> {
    pub(crate) fn new(listener: L) -> Accepting<L> {
        Accepting {
            listener,
            backoff: None,
            failures: 0,
            warned_at: None,
        }
    }

    fn out_of_resources(error: &io::Error) -> bool {
        error.raw_os_error().is_some_and(|code| {
            matches!(
                Errno::from_raw(code),
                Errno::EMFILE | Errno::ENFILE | Errno::ENOBUFS | Errno::ENOMEM
            )
        })
    }

    fn failed(&mut self, error: &io::Error) {
        self.failures += 1;
        if self.warned_at.is_none_or(|at| at.elapsed() >= WARN_EVERY) {
            tracing::warn!(
                error = %error,
                failures_count = self.failures,
                backoff_ms = ACCEPT_BACKOFF.as_millis(),
                "accept failed; backing off"
            );
            self.failures = 0;
            self.warned_at = Some(Instant::now());
        }
        self.backoff = Some(Box::pin(tokio::time::sleep(ACCEPT_BACKOFF)));
    }
}

impl<L: Listen + Unpin> Stream for Accepting<L> {
    type Item = io::Result<L::Stream>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(backoff) = self.backoff.as_mut() {
                ready!(backoff.as_mut().poll(cx));
                self.backoff = None;
            }
            match ready!(self.listener.poll_accept(cx)) {
                Ok(stream) => return Poll::Ready(Some(Ok(stream))),
                Err(error) if Self::out_of_resources(&error) => self.failed(&error),
                Err(error) => return Poll::Ready(Some(Err(error))),
            }
        }
    }
}
