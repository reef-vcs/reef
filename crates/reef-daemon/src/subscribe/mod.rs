//! Operation log subscriptions (R2.09, ADR 0036): `SubscribeOperations`.
//!
//! - [`events::OpEvents`] learns of every operation: the repository thread
//!   calls it after each job that moved the head, and a poll of the op
//!   heads catches other `jj` processes.
//! - A subscription's position is a [`reef_lib::jj::oplog::Frontier`] per
//!   repository, handed to clients as an opaque [`cursor`].
//! - `feed::read` reads what follows a position; R2.22's broker replaces
//!   its body and nothing else.
//! - `session` is the task behind each stream: frames, keepalives,
//!   backpressure, shutdown, and rewrites of the log.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::OperationIdHex;
use tokio::sync::Semaphore;

use crate::error::{Advice, Advise};
use crate::model::RepoId;

pub mod cursor;
pub mod events;
pub(crate) mod feed;
pub(crate) mod session;

pub use events::OpEvents;

/// What a subscription may cost and how its frames are cut. Not a
/// configuration-file key: every client relies on the same frames from
/// every daemon. Tests shrink them through [`crate::config::Config`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Operations a position may be behind the op heads, per repository.
    pub max_behind: usize,
    /// Operations in one frame at most.
    pub frame_operations: usize,
    /// Encoded bytes of one frame at most; one operation is never split.
    pub frame_bytes: usize,
    /// Least time between two live frames carrying operations.
    pub frame_interval: Duration,
    /// Uncached effects one frame's read computes at most.
    pub effects_per_frame: usize,
    /// An empty frame after this long without one.
    pub keepalive: Duration,
    /// How often the op heads of a followed repository are read, for
    /// operations other `jj` processes write.
    pub outside_poll: Duration,
    /// Subscriptions open at once, per daemon.
    pub max_subscriptions: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_behind: 10_000,
            frame_operations: 1_000,
            frame_bytes: 1 << 20,
            frame_interval: Duration::from_millis(50),
            effects_per_frame: 200,
            keepalive: Duration::from_secs(15),
            outside_poll: Duration::from_millis(200),
            max_subscriptions: 1_024,
        }
    }
}

/// The daemon's subscriptions; one in [`crate::AppState::subscriptions`].
#[derive(Debug)]
pub struct Subscriptions {
    /// Frame reads on the blocking pool at once, apart from `Reads`'
    /// permits, so a slow diff never delays delivery.
    pub(crate) permits: Arc<Semaphore>,
    active: Arc<AtomicUsize>,
}

impl Default for Subscriptions {
    fn default() -> Subscriptions {
        let parallel = std::thread::available_parallelism().map_or(2, std::num::NonZero::get);
        Subscriptions {
            permits: Arc::new(Semaphore::new(parallel.max(2))),
            active: Arc::new(AtomicUsize::new(0)),
        }
    }
}

impl Subscriptions {
    /// How many subscriptions are open.
    pub fn active(&self) -> usize {
        self.active.load(Ordering::Relaxed)
    }

    /// A slot for one more subscription, if fewer than `max` are open.
    pub(crate) fn admit(&self, max: usize) -> Result<Slot, SubscribeError> {
        self.active
            .try_update(Ordering::AcqRel, Ordering::Acquire, |active| {
                (active < max).then_some(active + 1)
            })
            .map_err(|_| SubscribeError::TooMany { limit: max })?;
        Ok(Slot(Arc::clone(&self.active)))
    }
}

/// One open subscription's place in the count, freed when dropped on any
/// exit path of its session.
#[derive(Debug)]
pub(crate) struct Slot(Arc<AtomicUsize>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Why a subscription cannot start or continue.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SubscribeError {
    /// A cursor that does not decode: reason `INVALID_CURSOR`.
    #[error("the cursor is not one a frame returned")]
    InvalidCursor,
    /// More than `limit` operations after the position: reason
    /// `TOO_FAR_BEHIND`.
    #[error("more than {limit} operations were recorded in repository {repo} after this position")]
    TooFarBehind {
        /// The repository.
        repo: RepoId,
        /// [`Limits::max_behind`].
        limit: usize,
    },
    /// The log was rewritten after the position: reason
    /// `OPERATION_LOG_REWRITTEN`.
    #[error("the operation log of repository {repo} was rewritten after this position")]
    Rewritten {
        /// The repository.
        repo: RepoId,
    },
    /// The daemon serves `limit` subscriptions already: reason
    /// `TOO_MANY_SUBSCRIPTIONS`.
    #[error("the daemon is serving its limit of {limit} subscriptions")]
    TooMany {
        /// [`Limits::max_subscriptions`].
        limit: usize,
    },
    /// `after_operation` with other than one repository.
    #[error("after_operation needs exactly one repository in repos")]
    AfterNeedsOneRepo,
    /// `after_operation` names an operation the op heads do not reach:
    /// reason `OPERATION_NOT_IN_HISTORY`.
    #[error("operation {0} is not in the history of the operation heads")]
    NotInHistory(OperationIdHex),
    /// A repository the request named was forgotten: reason
    /// `REPOSITORY_NOT_FOUND`.
    #[error("repository {0} was forgotten")]
    Forgotten(RepoId),
    /// The daemon is shutting down; no reason.
    #[error("the daemon is shutting down")]
    ShuttingDown,
}

impl Classify for SubscribeError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::InvalidCursor | Self::AfterNeedsOneRepo => ErrorKind::InvalidArgument,
            Self::TooFarBehind { .. } | Self::Rewritten { .. } | Self::NotInHistory(_) => {
                ErrorKind::Conflict
            }
            Self::TooMany { .. } | Self::ShuttingDown => ErrorKind::Unavailable,
            Self::Forgotten(_) => ErrorKind::NotFound,
        }
    }
}

impl Advise for SubscribeError {
    fn advice(&self) -> Option<Advice> {
        Some(Advice::hint(match self {
            Self::InvalidCursor => {
                "Pass a cursor exactly as a frame returned it, or subscribe without one.".to_owned()
            }
            Self::TooFarBehind { limit, .. } => format!(
                "More than {} operations were recorded after this point; read them with \
                 ListOperations, then subscribe without a start.",
                thousands(*limit)
            ),
            Self::Rewritten { .. } => "This repository's operation log was rewritten (garbage \
                                       collection) since this point; read the state again and \
                                       subscribe without a start."
                .to_owned(),
            Self::TooMany { limit } => format!(
                "The daemon is serving its limit of {} subscriptions; close one and \
                 subscribe again.",
                thousands(*limit)
            ),
            Self::AfterNeedsOneRepo => {
                "after_operation needs exactly one repository in repos; resume several with \
                 cursor."
                    .to_owned()
            }
            Self::ShuttingDown => "The daemon is shutting down; subscribe again with the last \
                                   frame's cursor when it is back."
                .to_owned(),
            Self::NotInHistory(_) | Self::Forgotten(_) => return None,
        }))
    }
}

/// `n` with commas between thousands, as the hints write numbers.
fn thousands(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, digit) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints_write_thousands() {
        assert_eq!(thousands(0), "0");
        assert_eq!(thousands(999), "999");
        assert_eq!(thousands(1_024), "1,024");
        assert_eq!(thousands(10_000), "10,000");
        assert_eq!(thousands(1_234_567), "1,234,567");
    }
}
