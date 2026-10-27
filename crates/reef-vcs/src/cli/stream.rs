//! What `reef op watch` (R2.09) and `reef presence watch` (R2.10) share:
//! ending by itself, reconnecting after the daemon goes away, and noticing
//! a closed stdout.

use std::time::Duration;

use reef_client::ClientError;
use reef_error::ErrorKind;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time::Instant;

use super::CliError;

/// Reconnection backoff, doubling between these.
pub(crate) const RECONNECT_FIRST: Duration = Duration::from_millis(100);
pub(crate) const RECONNECT_MAX: Duration = Duration::from_secs(2);
/// How long `--follow` keeps reconnecting before it fails.
pub(crate) const RECONNECT_FOLLOW: Duration = Duration::from_secs(60);

/// Why a watch stopped.
pub(crate) enum Stopped {
    /// `--count` reached, a signal, or a closed stdout.
    Done,
    /// The time ran out.
    Time,
}

/// Resolves at `deadline`; never without one.
pub(crate) async fn until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Runs `watch` until it stops, or until SIGTERM, SIGINT or a closed
/// stdout, which end it as [`Stopped::Done`].
pub(crate) async fn run_until_stopped(
    watch: impl Future<Output = Result<Stopped, CliError>>,
) -> Result<Stopped, CliError> {
    let mut terminate = signal(SignalKind::terminate()).map_err(CliError::Signals)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(CliError::Signals)?;
    tokio::select! {
        stopped = watch => stopped,
        _ = terminate.recv() => Ok(Stopped::Done),
        _ = interrupt.recv() => Ok(Stopped::Done),
        () = stdout_closed() => Ok(Stopped::Done),
    }
}

/// Resolves once nobody reads stdout: a pipe whose reader exited. Checked
/// every 200 ms, so `op watch --follow | head -1` ends after its line even
/// when nothing more is recorded to write.
pub(crate) async fn stdout_closed() {
    use std::os::fd::AsFd as _;

    use nix::poll::{PollFd, PollFlags, PollTimeout, poll};
    loop {
        tokio::time::sleep(Duration::from_millis(200)).await;
        let stdout = std::io::stdout();
        // macOS reports the hang-up only to a poll that asks for POLLOUT.
        let mut fds = [PollFd::new(stdout.as_fd(), PollFlags::POLLOUT)];
        let closed = PollFlags::POLLERR | PollFlags::POLLHUP;
        if poll(&mut fds, PollTimeout::ZERO).is_ok_and(|ready| ready > 0)
            && fds[0]
                .revents()
                .is_some_and(|events| events.intersects(closed))
        {
            return;
        }
    }
}

/// Whether a watch resubscribes after `error`: the daemon went away
/// (unavailable with no reason), the connection failed, or the transport
/// broke mid-stream (a status the daemon did not write, with no
/// `ErrorDetail`, as tonic reports a reset connection).
pub(crate) fn reconnects(error: &CliError) -> bool {
    match error {
        CliError::Daemon(ClientError::Rpc {
            kind,
            reason,
            status,
            ..
        }) => {
            (*kind == ErrorKind::Unavailable && reason.is_empty())
                || reef_api::error::detail(status).is_none()
        }
        CliError::Daemon(ClientError::Connect { .. }) | CliError::Timeout { .. } => true,
        _ => false,
    }
}

/// Backoff between reconnections, and when a run of failures began.
pub(crate) struct Backoff {
    next: Duration,
    failing_since: Option<Instant>,
}

impl Backoff {
    pub(crate) fn new() -> Backoff {
        Backoff {
            next: RECONNECT_FIRST,
            failing_since: None,
        }
    }

    /// A frame arrived: the next failure starts over.
    pub(crate) fn reset(&mut self) {
        self.next = RECONNECT_FIRST;
        self.failing_since = None;
    }

    /// Whether to reconnect after `error`: one a watch reconnects after,
    /// and, without a deadline (`--follow`), within [`RECONNECT_FOLLOW`] of
    /// the first failure in a row.
    pub(crate) fn retry(&mut self, error: &CliError, deadline: Option<Instant>) -> bool {
        let since = *self.failing_since.get_or_insert_with(Instant::now);
        reconnects(error) && (deadline.is_some() || since.elapsed() < RECONNECT_FOLLOW)
    }

    /// Waits the backoff, or until `deadline`; `false` at the deadline.
    pub(crate) async fn wait(&mut self, deadline: Option<Instant>) -> bool {
        let waited = tokio::select! {
            () = until(deadline) => false,
            () = tokio::time::sleep(self.next) => true,
        };
        self.next = (self.next * 2).min(RECONNECT_MAX);
        waited
    }
}
