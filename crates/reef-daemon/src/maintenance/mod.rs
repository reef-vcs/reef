//! Maintenance of the daemon's repositories and state: garbage collection
//! ([`gc`], R1.17), backup ([`backup`]) and quiesce ([`quiesce`]), and the
//! offline restore ([`restore`]) (R1.18, ADR 0020).
//!
//! Every maintenance task holds the daemon's one [`Lock`]. A second task
//! asked for while one runs is refused (`Unavailable`, "maintenance in
//! progress") rather than queued: a client that wants it can retry, and
//! nothing waits behind an hour-long GC. A quiesce holds the lock until it
//! resumes, so GC and backup wait it out the same way. The GC timer
//! ([`timer`], #95, ADR 0041) is a maintenance task too: it takes the lock
//! for each run and, finding it held, tries again later. A refusal names
//! the task that holds the lock.

use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

pub mod backup;
pub mod gc;
pub mod quiesce;
pub mod restore;
pub mod timer;

/// The daemon's maintenance lock, in [`crate::AppState::maintenance`].
#[derive(Debug, Default)]
pub struct Lock {
    lock: Arc<tokio::sync::Mutex<()>>,
    holder: Arc<Mutex<Option<Task>>>,
}

/// A maintenance task, named in [`Busy`] while it holds the lock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Task {
    /// `Maintenance.CollectGarbage` (`reef ws gc`).
    Gc,
    /// The daemon's own GC run, every `gc.interval`.
    TimedGc,
    /// `Maintenance.Backup` (`reef backup`).
    Backup,
    /// `Maintenance.Quiesce`, held until it resumes.
    Quiesce,
}

impl fmt::Display for Task {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Task::Gc => "a GC",
            Task::TimedGc => "a timed GC",
            Task::Backup => "a backup",
            Task::Quiesce => "a quiesce",
        })
    }
}

/// Proof that the maintenance lock is held; dropping it releases the lock.
#[must_use = "dropping the guard releases the maintenance lock at once"]
#[derive(Debug)]
pub struct Guard {
    holder: Arc<Mutex<Option<Task>>>,
    #[allow(dead_code, reason = "held for its drop")]
    lock: tokio::sync::OwnedMutexGuard<()>,
}

impl Drop for Guard {
    // Runs before any field drops, so the holder is cleared while the lock
    // is still held, and a newer holder's name is never wiped.
    fn drop(&mut self) {
        *self.holder.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }
}

impl Lock {
    /// A free lock.
    pub fn new() -> Lock {
        Lock::default()
    }

    /// Takes the lock for `task` if no maintenance task holds it. The guard
    /// does not borrow the lock, so a quiesce can keep it past the request
    /// that took it.
    pub fn try_acquire(&self, task: Task) -> Result<Guard, Busy> {
        match Arc::clone(&self.lock).try_lock_owned() {
            Ok(lock) => {
                *self.holder.lock().unwrap_or_else(PoisonError::into_inner) = Some(task);
                Ok(Guard {
                    holder: Arc::clone(&self.holder),
                    lock,
                })
            }
            // Read between another task taking the lock and naming itself,
            // the holder is unknown, and the message says only "in progress".
            Err(_) => Err(Busy {
                holder: *self.holder.lock().unwrap_or_else(PoisonError::into_inner),
            }),
        }
    }
}

/// A maintenance task was asked for while another one ran.
#[derive(Debug, thiserror::Error)]
#[error(
    "maintenance in progress{}; try again when it finishes",
    holder.map(|task| format!(" ({task})")).unwrap_or_default()
)]
pub struct Busy {
    /// The task holding the lock, when known.
    pub holder: Option<Task>,
}

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
        let held = lock.try_acquire(Task::Gc).unwrap();
        assert!(lock.try_acquire(Task::Backup).is_err());
        drop(held);
        assert!(lock.try_acquire(Task::Backup).is_ok());
    }

    #[test]
    fn busy_names_the_holder() {
        let lock = Lock::new();
        let held = lock.try_acquire(Task::TimedGc).unwrap();
        let busy = lock.try_acquire(Task::Gc).unwrap_err();
        assert_eq!(busy.holder, Some(Task::TimedGc));
        assert_eq!(
            busy.to_string(),
            "maintenance in progress (a timed GC); try again when it finishes"
        );
        drop(held);
        assert!(lock.holder.lock().unwrap().is_none());
        assert!(lock.try_acquire(Task::Gc).is_ok());
        assert_eq!(
            Busy { holder: None }.to_string(),
            "maintenance in progress; try again when it finishes"
        );
    }
}
