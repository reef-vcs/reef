//! The follower (R3.27): one task that keeps every repository's
//! assignments in step with its operation log.
//!
//! It is woken by every new operation ([`crate::subscribe::OpEvents`]),
//! by [`super::Assignments::wake`], and by the earliest grace deadline the
//! last looks saw (at least once a minute), settles 200 ms, and looks at
//! every open repository ([`super::look_loaded`]), no more than once a
//! second, at the head the daemon has loaded. A look reads jj again only
//! when that head moved; the owners' liveness is read from the database
//! every time.

use std::sync::{Arc, Weak};
use std::time::Duration;

use reef_lib::error::ErrorChain;

use crate::daemon::AppState;
use crate::model::{RepoId, Timestamp};

/// How long the task waits after a wake-up, so a burst of operations is
/// looked at once.
const SETTLE: Duration = Duration::from_millis(200);
/// The shortest time between two passes.
const INTERVAL: Duration = Duration::from_secs(1);
/// The longest time between two passes.
const IDLE: Duration = Duration::from_mins(1);

/// Starts the task; it stops at shutdown.
pub(crate) fn start(state: &Arc<AppState>) {
    let weak = Arc::downgrade(state);
    let shutdown = state.shutdown.clone();
    let mut wakes = state.events.changes();
    tokio::spawn(async move {
        loop {
            let started = tokio::time::Instant::now();
            let wait = match weak.upgrade() {
                Some(state) => {
                    pass(&state).await;
                    until_deadline(&state)
                }
                None => return,
            };
            tokio::select! {
                () = shutdown.cancelled() => return,
                changed = wakes.changed() => {
                    if changed.is_err() {
                        return;
                    }
                }
                () = wake(&weak) => {}
                () = tokio::time::sleep(wait) => {}
            }
            let since = started.elapsed();
            let pause = SETTLE.max(INTERVAL.saturating_sub(since));
            tokio::select! {
                () = shutdown.cancelled() => return,
                () = tokio::time::sleep(pause) => {}
            }
        }
    });
}

/// Waits for [`super::Assignments::wake`]; holds the state while it
/// waits, which shutdown's cancellation ends.
async fn wake(weak: &Weak<AppState>) {
    let Some(state) = weak.upgrade() else {
        return std::future::pending().await;
    };
    state.assignments.wake.notified().await;
}

/// How long until the earliest grace deadline, at most [`IDLE`].
fn until_deadline(state: &AppState) -> Duration {
    match state.assignments.deadline() {
        Some(at) => {
            let left = at.0.saturating_sub(Timestamp::now().0).max(0);
            Duration::from_millis(u64::try_from(left).unwrap_or(0)).min(IDLE)
        }
        None => IDLE,
    }
}

/// Looks at every open repository once.
async fn pass(state: &Arc<AppState>) {
    state.assignments.clear_deadline();
    for repo in state.repos.ids() {
        if let Err(error) = super::look_loaded(state, repo).await {
            tracing::warn!(%repo, error = %ErrorChain(&error), "cannot bring conflict assignments up to date");
        }
    }
}

/// Looks once, now, at `repo` (tests).
#[doc(hidden)]
pub async fn look_now(state: &Weak<AppState>, repo: RepoId) -> Result<(), String> {
    let Some(state) = state.upgrade() else {
        return Ok(());
    };
    super::look(&state, repo)
        .await
        .map_err(|error| ErrorChain(&error).to_string())
}
