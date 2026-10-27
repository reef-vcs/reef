//! Presence (R2.10, ADR 0037): ephemeral state about who is doing what,
//! beside the operation log and never persisted.
//!
//! - [`PresenceHub`] holds the live entries in memory, expires them, and
//!   broadcasts their changes, coalesced per entry.
//! - `session` is the task behind each `SubscribePresence` stream: the
//!   current set, then changes, latest state per entry.
//! - [`publish`] holds what the daemon reports itself: lease heartbeats,
//!   files snapshots change, and the progress of creates and store builds.

use std::time::Duration;

use reef_lib::error::{Classify, ErrorKind};

use crate::error::{Advice, Advise};
use crate::model::PresenceId;

mod filter;
mod hub;
pub mod publish;
pub(crate) mod session;

pub use filter::PresenceFilter;
pub use hub::{Change, DaemonKey, PresenceHub, Report};

/// A TTL left unset or zero.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);
/// The shortest TTL; a shorter one is raised to it.
pub const MIN_TTL: Duration = Duration::from_secs(1);
/// The longest TTL; a longer one is lowered to it.
pub const MAX_TTL: Duration = Duration::from_secs(600);
/// `activity` in bytes at most.
pub const MAX_ACTIVITY_BYTES: usize = 1_024;
/// `paths` at most.
pub const MAX_PATHS: usize = 64;
/// `progress.phase` and `progress.unit` in bytes at most.
pub const MAX_LABEL_BYTES: usize = 64;
/// One encoded entry in bytes at most.
pub const MAX_ENTRY_BYTES: usize = 4_096;
/// Ended entries whose end reason the hub remembers, for a resync.
pub(crate) const ENDED_RING: usize = 4_096;
/// Ended workspaces, and forgotten repositories, the hub remembers, so a
/// report that raced the end is refused.
pub(crate) const ENDED_WORKSPACES: usize = 1_024;

/// What presence may cost and how its frames are cut. Not a
/// configuration-file key: every client relies on the same behavior from
/// every daemon. Tests shrink them through [`crate::config::Config`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Client entries per daemon at most.
    pub max_entries: usize,
    /// Client entries per agent at most.
    pub max_entries_per_agent: usize,
    /// Least time between two broadcasts of one entry's changes.
    pub publish_interval: Duration,
    /// Least time between two frames carrying events.
    pub frame_interval: Duration,
    /// Events in one frame at most.
    pub frame_events: usize,
    /// Encoded bytes of one frame at most; one event is never split.
    pub frame_bytes: usize,
    /// Changes a subscription may fall behind before it resynchronizes.
    pub backlog: usize,
    /// An empty frame after this long without one.
    pub keepalive: Duration,
    /// How long an operation runs before its progress is published.
    pub progress_after: Duration,
    /// How long a workspace's editing entry lasts after its last snapshot.
    pub editing_ttl: Duration,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            max_entries: 4_096,
            max_entries_per_agent: 32,
            publish_interval: Duration::from_millis(50),
            frame_interval: Duration::from_millis(50),
            frame_events: 1_000,
            frame_bytes: 1 << 20,
            backlog: 4_096,
            keepalive: Duration::from_secs(15),
            progress_after: Duration::from_millis(250),
            editing_ttl: Duration::from_secs(60),
        }
    }
}

/// `ttl` as stored: unset or zero is [`DEFAULT_TTL`], and the rest is
/// clamped to [`MIN_TTL`]..=[`MAX_TTL`].
pub fn clamp_ttl(ttl: Option<Duration>) -> Duration {
    match ttl {
        None => DEFAULT_TTL,
        Some(ttl) if ttl.is_zero() => DEFAULT_TTL,
        Some(ttl) => ttl.clamp(MIN_TTL, MAX_TTL),
    }
}

/// Why a presence report, clear, or subscription is refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PresenceError {
    /// `presence_id` names no live entry: reason `PRESENCE_NOT_FOUND`.
    #[error("presence entry {0} is not live: cleared, expired, or reported before a restart")]
    NotFound(PresenceId),
    /// `presence_id` is not an id: reason `INVALID_PRESENCE_ID`.
    #[error("invalid presence_id {0:?}: expected 16 hex characters")]
    InvalidId(String),
    /// A replacement changes a fixed field: reason `PRESENCE_MISMATCH`.
    #[error("presence entry {id}'s {field} is fixed; report a new entry")]
    Mismatch {
        /// The entry.
        id: PresenceId,
        /// `kind`, `agent`, `repo` or `workspace`.
        field: &'static str,
    },
    /// A report of kind `LEASE`: reason `DAEMON_PRESENCE`, metadata
    /// `kind`.
    #[error("lease presence is reported by the daemon only")]
    DaemonKind,
    /// A report or clear of an entry the daemon reports: reason
    /// `DAEMON_PRESENCE`, metadata `presence_id`.
    #[error("presence entry {0} is reported by the daemon")]
    DaemonEntry(PresenceId),
    /// A field over its bound: reason `PRESENCE_TOO_LARGE`.
    #[error("presence {field} is over its limit of {limit}")]
    TooLarge {
        /// The field.
        field: &'static str,
        /// The bound, as the hint writes it.
        limit: String,
    },
    /// The caps are reached: reason `TOO_MANY_PRESENCE_ENTRIES`.
    #[error("{}", match agent {
        Some(agent) => format!("agent {agent:?} holds its limit of {limit} presence entries"),
        None => format!("the daemon holds its limit of {limit} presence entries"),
    })]
    TooMany {
        /// The cap reached.
        limit: usize,
        /// The agent, for the cap per agent.
        agent: Option<String>,
    },
    /// An `agent` that is empty, not visible ASCII, or longer than 256
    /// bytes: reason `INVALID_AGENT`.
    #[error(
        "agent must be visible ASCII of 1 to {} bytes",
        reef_api::AGENT_MAX_BYTES
    )]
    InvalidAgent,
    /// A malformed request with no reason: an unspecified kind, a field
    /// another kind carries, a negative TTL, or a change without a
    /// repository.
    #[error("{0}")]
    Invalid(String),
    /// The workspace ended, or the repository was forgotten, while the
    /// report was made.
    #[error("the workspace or repository the entry names is gone")]
    Ended,
    /// The daemon is shutting down; no reason.
    #[error("the daemon is shutting down")]
    ShuttingDown,
}

impl Classify for PresenceError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotFound(_) => ErrorKind::NotFound,
            Self::InvalidId(_)
            | Self::Mismatch { .. }
            | Self::DaemonKind
            | Self::DaemonEntry(_)
            | Self::TooLarge { .. }
            | Self::InvalidAgent
            | Self::Invalid(_) => ErrorKind::InvalidArgument,
            Self::Ended => ErrorKind::Conflict,
            Self::TooMany { .. } | Self::ShuttingDown => ErrorKind::Unavailable,
        }
    }
}

impl Advise for PresenceError {
    fn advice(&self) -> Option<Advice> {
        Some(Advice::hint(match self {
            Self::NotFound(_) => {
                "The entry was cleared, expired, or the daemon restarted; report it again \
                 without presence_id."
            }
            Self::InvalidId(_) => "Pass presence_id exactly as ReportPresence returned it.",
            Self::Mismatch { .. } => {
                "An entry's kind, agent, repo and workspace are fixed; report a new entry for \
                 different ones."
            }
            Self::DaemonKind | Self::DaemonEntry(_) => {
                "The daemon reports this entry itself; it cannot be reported or cleared by a \
                 client."
            }
            Self::TooLarge { .. } => {
                "Shorten it: activity 1,024 bytes, 64 paths, phase and unit 64 bytes, 4 KiB in \
                 all."
            }
            Self::TooMany { .. } => {
                "Clear entries you no longer need, or replace one by its presence_id instead of \
                 reporting a new one."
            }
            Self::Ended => {
                "The workspace was released or reclaimed, or its repository forgotten; report \
                 without it."
            }
            Self::ShuttingDown => {
                "The daemon is shutting down; subscribe again when it is back; the set you \
                 receive then replaces what you held."
            }
            Self::InvalidAgent | Self::Invalid(_) => return None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ttls_are_clamped() {
        assert_eq!(clamp_ttl(None), DEFAULT_TTL);
        assert_eq!(clamp_ttl(Some(Duration::ZERO)), DEFAULT_TTL);
        assert_eq!(clamp_ttl(Some(Duration::from_millis(100))), MIN_TTL);
        assert_eq!(clamp_ttl(Some(Duration::from_mins(20))), MAX_TTL);
        assert_eq!(
            clamp_ttl(Some(Duration::from_secs(45))),
            Duration::from_secs(45)
        );
    }
}
