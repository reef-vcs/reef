//! Maintenance of the daemon's repositories and state: garbage collection
//! ([`gc`], R1.17), backup ([`backup`]) and quiesce ([`quiesce`]), and the
//! offline restore ([`restore`]) (R1.18, ADR 0020).
//!
//! Every maintenance task holds the daemon's one [`Lock`]. A second task
//! asked for while one runs is refused (`Unavailable`, "maintenance in
//! progress") rather than queued: a client that wants it can retry, and
//! nothing waits behind an hour-long GC. A quiesce holds the lock until it
//! resumes, so GC and backup wait it out the same way.

use std::sync::Arc;

pub mod backup;
pub mod gc;
pub mod quiesce;
pub mod restore;

/// The daemon's maintenance lock, in [`crate::AppState::maintenance`].
#[derive(Debug, Default)]
pub struct Lock(Arc<tokio::sync::Mutex<()>>);

/// Proof that the maintenance lock is held; dropping it releases the lock.
#[must_use = "dropping the guard releases the maintenance lock at once"]
#[derive(Debug)]
pub struct Guard(
    #[allow(dead_code, reason = "held for its drop")] tokio::sync::OwnedMutexGuard<()>,
);

impl Lock {
    /// A free lock.
    pub fn new() -> Lock {
        Lock::default()
    }

    /// Takes the lock if no maintenance task holds it. The guard does not
    /// borrow the lock, so a quiesce can keep it past the request that took
    /// it.
    pub fn try_acquire(&self) -> Option<Guard> {
        Arc::clone(&self.0).try_lock_owned().ok().map(Guard)
    }
}

/// A maintenance task was asked for while another one ran.
#[derive(Debug, thiserror::Error)]
#[error("maintenance in progress; try again when it finishes")]
pub struct Busy;

impl reef_lib::error::Classify for Busy {
    fn kind(&self) -> reef_lib::error::ErrorKind {
        reef_lib::error::ErrorKind::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn second_acquire_fails_until_the_first_is_dropped() {
        let lock = Lock::new();
        let held = lock.try_acquire().unwrap();
        assert!(lock.try_acquire().is_none());
        drop(held);
        assert!(lock.try_acquire().is_some());
    }
}
