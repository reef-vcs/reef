//! Maintenance of the daemon's repositories and state: garbage collection
//! ([`gc`], R1.17), and R1.18's backup and quiesce.
//!
//! Every maintenance task holds the daemon's one [`Lock`]. A second task
//! asked for while one runs is refused (`Unavailable`, "maintenance in
//! progress") rather than queued: a client that wants it can retry, and
//! nothing waits behind an hour-long GC.

pub mod gc;

/// The daemon's maintenance lock, in [`crate::AppState::maintenance`].
#[derive(Debug, Default)]
pub struct Lock(tokio::sync::Mutex<()>);

/// Proof that the maintenance lock is held; dropping it releases the lock.
#[must_use = "dropping the guard releases the maintenance lock at once"]
#[derive(Debug)]
pub struct Guard<'a>(
    #[allow(dead_code, reason = "held for its drop")] tokio::sync::MutexGuard<'a, ()>,
);

impl Lock {
    /// A free lock.
    pub fn new() -> Lock {
        Lock::default()
    }

    /// Takes the lock if no maintenance task holds it.
    pub fn try_acquire(&self) -> Option<Guard<'_>> {
        self.0.try_lock().ok().map(Guard)
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
