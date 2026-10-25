//! The daemon's record types: what [`crate::db`] stores about repositories,
//! workspaces, leases, and reclaims, and the ids that name them.
//!
//! The string forms of [`WorkspaceState`], [`LeaseEnd`], [`ReclaimReason`],
//! and [`WatcherKind`] are the values the `CHECK` constraints in
//! `db/migrations/` allow; a test keeps the two in step.

pub mod ids;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use ids::{LeaseId, ParseIdError, RepoId, WorkspaceId};

/// A stored name that is not one of an enum's values.
#[derive(Debug, thiserror::Error)]
#[error("unknown {what} {value:?}")]
pub struct UnknownName {
    /// The kind of value expected, such as `workspace state`.
    pub what: &'static str,
    /// The rejected text.
    pub value: String,
}

/// Defines `as_str`, `parse`, `ALL`, and the SQL conversions of an enum
/// stored as text.
macro_rules! sql_enum {
    ($name:ident, $what:literal, { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &[Self] = &[$(Self::$variant),+];

            /// The stored name.
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// The value whose stored name is `text`, if any.
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                Ok(ToSqlOutput::from(self.as_str()))
            }
        }

        impl FromSql for $name {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                let text = value.as_str()?;
                Self::parse(text).ok_or_else(|| {
                    FromSqlError::Other(Box::new(UnknownName {
                        what: $what,
                        value: text.to_owned(),
                    }))
                })
            }
        }
    };
}

/// Where a workspace is in its life. Rows are never deleted by a state
/// change: a workspace moves `active → releasing → released`, or to
/// `reclaimed` from either of the first two, and R1.17 deletes old terminal
/// rows.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    /// Leased and usable.
    Active,
    /// Its owner asked to release it and the final snapshot and tear-down
    /// are in progress.
    Releasing,
    /// Released by its owner; the directory is gone.
    Released,
    /// Reclaimed by the daemon (lease expiry, GC, restore); the directory is
    /// gone.
    Reclaimed,
}

sql_enum!(WorkspaceState, "workspace state", {
    Active => "active",
    Releasing => "releasing",
    Released => "released",
    Reclaimed => "reclaimed",
});

impl WorkspaceState {
    /// Whether the workspace is finished: its directory is gone and its row
    /// is only history.
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Released | Self::Reclaimed)
    }

    /// Whether a workspace in this state may move to `next`. Staying in the
    /// same state is allowed. A terminal state is final, and `released` is
    /// reached only through `releasing`. `releasing → active` is allowed so
    /// a release that fails before tear-down can be rolled back.
    pub fn can_become(self, next: Self) -> bool {
        use WorkspaceState::{Active, Reclaimed, Released, Releasing};
        self == next
            || matches!(
                (self, next),
                (Active, Releasing | Reclaimed) | (Releasing, Active | Released | Reclaimed)
            )
    }
}

/// Why a workspace needs someone to look at it, shown by `ws list` and
/// `ws inspect`. Stored in `workspaces.attention`, which has no `CHECK`
/// constraint so later tickets can add reasons without a migration.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Attention {
    /// Another workspace rewrote this workspace's change while it had local
    /// edits; the change is divergent (R1.13).
    Divergent,
    /// The working-copy commit vanished from the view and the workspace
    /// recovered onto a new change (R1.13).
    StaleRecovered,
    /// The final snapshot before a reclaim failed; the directory is kept
    /// (R1.15).
    FinalSnapshotFailed,
    /// A mounted shared store no longer matches its lockfile hash (R1.20).
    StoreHashMismatch,
    /// The workspace was created without a shared store it declares (R1.20).
    StoreMissing,
    /// The filesystem watcher for the workspace failed (R1.13).
    WatchFailed,
    /// An overlay workspace's upper directory is missing (R1.09).
    UpperMissing,
    /// An overlay workspace could not be mounted again at daemon start; it
    /// is not snapshotted until it is (R1.09).
    MountFailed,
}

sql_enum!(Attention, "attention reason", {
    Divergent => "divergent",
    StaleRecovered => "stale-recovered",
    FinalSnapshotFailed => "final-snapshot-failed",
    StoreHashMismatch => "store-hash-mismatch",
    StoreMissing => "store-missing",
    WatchFailed => "watch-failed",
    UpperMissing => "upper-missing",
    MountFailed => "mount-failed",
});

/// Why a lease ended. A workspace move keeps its lease (R1.06), so moving is
/// not a reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseEnd {
    /// The owner released the workspace.
    Released,
    /// The lease passed its expiry and grace without a heartbeat.
    Expired,
    /// A restore from backup replaced the lease (R1.18).
    Restored,
}

sql_enum!(LeaseEnd, "lease end reason", {
    Released => "released",
    Expired => "expired",
    Restored => "restored",
});

/// Why a workspace's directory was reclaimed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReclaimReason {
    /// Its owner released it.
    Released,
    /// Its lease expired.
    Expired,
    /// Garbage collection removed it (R1.17).
    Gc,
    /// A restore from backup removed it (R1.18).
    Restore,
}

sql_enum!(ReclaimReason, "reclaim reason", {
    Released => "released",
    Expired => "expired",
    Gc => "gc",
    Restore => "restore",
});

/// What triggers a workspace's snapshots (R1.13, R1.14). Fixed at create
/// and stored in `workspaces.watcher`: a Watchman workspace's working copy
/// also narrows its scans with Watchman, which jj fixes when it loads the
/// working copy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WatcherKind {
    /// The `notify` crate: inotify on Linux, `FSEvents` on macOS.
    #[default]
    Notify,
    /// A Watchman subscription, and Watchman-narrowed scans.
    Watchman,
}

sql_enum!(WatcherKind, "watcher", {
    Notify => "notify",
    Watchman => "watchman",
});

/// A registered repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryRecord {
    /// The repository's id.
    pub id: RepoId,
    /// The root of the user's own jj workspace, by which it was registered.
    pub root: PathBuf,
    /// Its `.jj/repo` directory.
    pub jj_repo_path: PathBuf,
    /// Whether jj and git share the working copy.
    pub colocated: bool,
    /// When it was registered.
    pub registered_at: Timestamp,
    /// Files in its `default` workspace's working-copy commit, counted at
    /// registration (or the first daemon start after R1.14); `None` until
    /// counted.
    pub tracked_files: Option<u64>,
}

/// A Reef workspace: a leased directory backed by its own jj workspace
/// (ADR 0014).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkspaceRecord {
    /// The workspace's id.
    pub id: WorkspaceId,
    /// The repository it belongs to.
    pub repo_id: RepoId,
    /// Who holds it: an opaque string the client supplies.
    pub owner: String,
    /// What it is for, free text; empty if not given.
    pub purpose: String,
    /// The jj change it is on, in hex.
    pub change_id: String,
    /// Its working-copy commit as of the last snapshot or check-out.
    pub commit_id: String,
    /// The tree its directory was materialized from.
    pub base_tree_id: String,
    /// The materialization backend's name.
    pub backend: String,
    /// Its directory, canonical.
    pub path: PathBuf,
    /// The jj working-copy state directory, outside the tree.
    pub state_dir: PathBuf,
    /// Its unended lease, if it has one.
    pub lease_id: Option<LeaseId>,
    /// Where it is in its life.
    pub state: WorkspaceState,
    /// Whether its change is divergent.
    pub divergent: bool,
    /// Why it needs attention, if it does.
    pub attention: Option<Attention>,
    /// When it was created.
    pub created_at: Timestamp,
    /// When its last snapshot was taken.
    pub last_snapshot_at: Option<Timestamp>,
    /// The operation that recorded its last snapshot.
    pub last_snapshot_op: Option<String>,
    /// What triggers its snapshots.
    pub watcher: WatcherKind,
}

/// One lease on a workspace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeaseRecord {
    /// The lease's id, which heartbeats name.
    pub id: LeaseId,
    /// The repository of the leased workspace.
    pub repo_id: RepoId,
    /// The leased workspace.
    pub workspace_id: WorkspaceId,
    /// Who holds it.
    pub owner: String,
    /// When it was issued.
    pub issued_at: Timestamp,
    /// When it lapses unless renewed.
    pub expires_at: Timestamp,
    /// When it was last issued or renewed.
    pub last_heartbeat_at: Timestamp,
    /// How far each heartbeat extends it.
    pub length: Duration,
    /// How long after `expires_at` a heartbeat still revives it.
    pub grace: Duration,
    /// When it ended, if it has.
    pub ended_at: Option<Timestamp>,
    /// Why it ended, if it has.
    pub end_reason: Option<LeaseEnd>,
}

/// One entry of the reclaim log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReclaimRecord {
    /// The entry's row id, assigned on insert.
    pub id: i64,
    /// The repository of the reclaimed workspace.
    pub repo_id: RepoId,
    /// The reclaimed workspace.
    pub workspace_id: WorkspaceId,
    /// Why it was reclaimed.
    pub reason: ReclaimReason,
    /// The operation holding its final snapshot, if one was written.
    pub final_op_id: Option<String>,
    /// Whether a final snapshot was taken (it may have found no changes).
    pub snapshot_taken: bool,
    /// Bytes freed on disk, if measured.
    pub freed_bytes: Option<u64>,
    /// When it was reclaimed.
    pub at: Timestamp,
    /// Free-text detail, such as an error that was tolerated.
    pub detail: Option<String>,
}

/// One base cache entry: a tree materialized under the repository's
/// `bases/` directory (R1.02). Its jj `tree_state` is in the sibling
/// directory `<path>.state`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BaseCacheRecord {
    /// The repository the tree belongs to.
    pub repo_id: RepoId,
    /// The base cache key: the tree id in hex, or a conflicted tree's ids
    /// joined with `+` (`reef_lib::jj::materialize::tree_key`).
    pub tree_id: String,
    /// The materialized directory, `<state>/repos/<repo-id>/bases/<tree-id>`.
    pub path: PathBuf,
    /// Total size of its files as jj recorded them.
    pub bytes: u64,
    /// Number of files and symlinks in it.
    pub files: u64,
    /// When it was materialized.
    pub created_at: Timestamp,
    /// When a workspace was last materialized from it.
    pub last_used_at: Timestamp,
}

/// A row of `stores` (R1.20): a shared dependency store built from one
/// lockfile, under `<state>/stores/<name>/<lockfile-hash>/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreRecord {
    /// The repository that first built it; informational, since stores are
    /// shared by every repository on the daemon.
    pub repo_id: RepoId,
    /// The store's name in `reef.toml`.
    pub name: String,
    /// blake3 of the lockfile's bytes, 64 lowercase hex characters.
    pub lockfile_hash: String,
    /// Its directory.
    pub path: PathBuf,
    /// When its build succeeded; `None` while it builds.
    pub built_at: Option<Timestamp>,
    /// Whether it is built and read-only: what a workspace may mount.
    pub sealed: bool,
}

/// A point in time as Unix milliseconds UTC, the form SQLite stores. Shown as
/// RFC 3339 at the edges.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Timestamp(pub i64);

impl Timestamp {
    /// The current time.
    pub fn now() -> Self {
        Self::from(SystemTime::now())
    }

    /// The time as RFC 3339 UTC with millisecond precision, such as
    /// `2026-10-16T18:31:17.042Z`. A value outside the years 1 to 9999
    /// cannot be written that way and prints as `<millis>ms` instead.
    pub fn to_rfc3339(self) -> String {
        reef_api::json::rfc3339_millis(self.0)
    }
}

impl From<OffsetDateTime> for Timestamp {
    fn from(time: OffsetDateTime) -> Self {
        let millis = time.unix_timestamp_nanos() / 1_000_000;
        // `OffsetDateTime` spans at most ±9999 years, well inside i64 milliseconds.
        Self(i64::try_from(millis).unwrap_or(if millis < 0 { i64::MIN } else { i64::MAX }))
    }
}

impl From<SystemTime> for Timestamp {
    fn from(time: SystemTime) -> Self {
        match time.duration_since(UNIX_EPOCH) {
            Ok(after) => Self(duration_millis(after)),
            Err(before) => Self(duration_millis(before.duration()).saturating_neg()),
        }
    }
}

impl std::fmt::Display for Timestamp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_rfc3339())
    }
}

impl ToSql for Timestamp {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::from(self.0))
    }
}

impl FromSql for Timestamp {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        value.as_i64().map(Self)
    }
}

/// `duration` in whole milliseconds, saturating at `i64::MAX`.
pub(crate) fn duration_millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    /// The quoted values of every `CHECK (<column> IN (...))` in the
    /// migrations, by column.
    fn check_sets() -> BTreeMap<String, BTreeSet<String>> {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/db/migrations");
        let mut sql = String::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            sql.push_str(&std::fs::read_to_string(entry.unwrap().path()).unwrap());
        }
        let mut sets = BTreeMap::new();
        for (index, _) in sql.match_indices("CHECK (") {
            let rest = &sql[index + "CHECK (".len()..];
            let (column, rest) = rest.split_once(" IN (").unwrap();
            let (list, _) = rest.split_once(')').unwrap();
            let values = list
                .split(',')
                .map(|value| value.trim().trim_matches('\'').to_owned())
                .collect();
            sets.insert(column.trim().to_owned(), values);
        }
        sets
    }

    fn names<T: Copy>(all: &[T], as_str: fn(T) -> &'static str) -> BTreeSet<String> {
        all.iter().map(|value| as_str(*value).to_owned()).collect()
    }

    #[test]
    fn check_constraints_match_the_enums() {
        let sets = check_sets();
        let expected = BTreeMap::from([
            (
                "state".to_owned(),
                names(WorkspaceState::ALL, WorkspaceState::as_str),
            ),
            (
                "end_reason".to_owned(),
                names(LeaseEnd::ALL, LeaseEnd::as_str),
            ),
            (
                "reason".to_owned(),
                names(ReclaimReason::ALL, ReclaimReason::as_str),
            ),
            (
                "watcher".to_owned(),
                names(WatcherKind::ALL, WatcherKind::as_str),
            ),
            (
                "kind".to_owned(),
                names(
                    crate::snapshot::SnapshotKind::ALL,
                    crate::snapshot::SnapshotKind::as_str,
                ),
            ),
        ]);
        assert_eq!(sets, expected);
    }

    #[test]
    fn names_parse_back() {
        for state in WorkspaceState::ALL {
            assert_eq!(WorkspaceState::parse(state.as_str()), Some(*state));
            let json = serde_json::to_string(state).unwrap();
            assert_eq!(json, format!("\"{}\"", state.as_str()));
        }
        for attention in Attention::ALL {
            assert_eq!(Attention::parse(attention.as_str()), Some(*attention));
            let json = serde_json::to_string(attention).unwrap();
            assert_eq!(json, format!("\"{}\"", attention.as_str()));
        }
        for end in LeaseEnd::ALL {
            assert_eq!(LeaseEnd::parse(end.as_str()), Some(*end));
        }
        for reason in ReclaimReason::ALL {
            assert_eq!(ReclaimReason::parse(reason.as_str()), Some(*reason));
        }
        assert_eq!(WorkspaceState::parse("Active"), None);
    }

    #[test]
    fn state_transitions() {
        use WorkspaceState::{Active, Reclaimed, Released, Releasing};
        assert!(Active.can_become(Releasing));
        assert!(Active.can_become(Reclaimed));
        assert!(Releasing.can_become(Released));
        assert!(Releasing.can_become(Reclaimed));
        assert!(Releasing.can_become(Active));
        assert!(!Active.can_become(Released));
        for terminal in [Released, Reclaimed] {
            assert!(terminal.is_terminal());
            for next in WorkspaceState::ALL {
                assert_eq!(terminal.can_become(*next), *next == terminal);
            }
        }
    }

    #[test]
    fn timestamps_convert_at_the_edges() {
        let time = time::macros::datetime!(2026-10-16 18:31:17.042 UTC);
        let stamp = Timestamp::from(time);
        assert_eq!(stamp, Timestamp(1_792_175_477_042));
        assert_eq!(stamp.to_rfc3339(), "2026-10-16T18:31:17.042Z");
        assert_eq!(Timestamp(i64::MAX).to_rfc3339(), format!("{}ms", i64::MAX));
        assert!(Timestamp::now() > stamp);
    }
}
