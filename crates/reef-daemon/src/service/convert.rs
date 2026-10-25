//! Conversions between the daemon's records and the generated messages,
//! one per type, so the manager never sees the generated code.

use std::time::Duration;

use crate::backend::DiskUsage;
use crate::lease::{LeaseManager, LeaseState};
use crate::model::{LeaseRecord, RepositoryRecord, Timestamp, WorkspaceRecord};
use crate::snapshot::index::SnapshotEntry;
use crate::store::{MountMode, MountedStore};
use crate::watcher::PendingWatch;
use crate::workspace::CreateTimings;
use reef_api::proto::v1;

/// Unix milliseconds as a `google.protobuf.Timestamp`.
pub(super) fn timestamp(time: Timestamp) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: time.0.div_euclid(1000),
        // In 0..1000 ms, so the product fits in an i32.
        nanos: i32::try_from(time.0.rem_euclid(1000) * 1_000_000).unwrap_or(0),
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

/// A duration as a `google.protobuf.Duration`.
pub(super) fn proto_duration(duration: Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        // Below one second, so it fits.
        nanos: i32::try_from(duration.subsec_nanos()).unwrap_or(0),
    }
}

impl From<&RepositoryRecord> for v1::Repository {
    fn from(record: &RepositoryRecord) -> Self {
        Self {
            id: record.id.as_hex(),
            root: record.root.display().to_string(),
            colocated: record.colocated,
            registered_at: Some(timestamp(record.registered_at)),
        }
    }
}

impl From<LeaseState> for v1::lease::State {
    fn from(state: LeaseState) -> Self {
        match state {
            LeaseState::Active => Self::Active,
            LeaseState::InGrace => Self::InGrace,
            LeaseState::Expired => Self::Expired,
            LeaseState::Ended => Self::Ended,
        }
    }
}

/// A lease as a message, its state computed at `now`.
pub(super) fn lease(lease: &LeaseRecord, leases: &LeaseManager, now: Timestamp) -> v1::Lease {
    v1::Lease {
        id: lease.id.as_hex(),
        owner: lease.owner.clone(),
        issued_at: Some(timestamp(lease.issued_at)),
        expires_at: Some(timestamp(lease.expires_at)),
        last_heartbeat_at: Some(timestamp(lease.last_heartbeat_at)),
        length_ms: millis(lease.length),
        grace_ms: millis(lease.grace),
        state: v1::lease::State::from(LeaseState::of(lease, now)).into(),
        end_reason: lease
            .end_reason
            .map(|reason| reason.as_str().to_owned())
            .unwrap_or_default(),
        heartbeat_every: Some(proto_duration(leases.heartbeat_every(lease.length))),
    }
}

/// A workspace row, its lease, its local snapshot count, its mounted
/// stores, and why its watch is not up yet, as one message.
pub(super) struct WorkspaceView<'a> {
    pub(super) record: &'a WorkspaceRecord,
    pub(super) lease: Option<v1::Lease>,
    pub(super) local_snapshot_count: u64,
    pub(super) stores: &'a [MountedStore],
    pub(super) watch_pending: Option<PendingWatch>,
}

impl From<WorkspaceView<'_>> for v1::Workspace {
    fn from(view: WorkspaceView<'_>) -> Self {
        let record = view.record;
        Self {
            id: record.id.as_hex(),
            repo_id: record.repo_id.as_hex(),
            owner: record.owner.clone(),
            purpose: record.purpose.clone(),
            change_id: record.change_id.clone(),
            commit_id: record.commit_id.clone(),
            backend: record.backend.clone(),
            path: record.path.display().to_string(),
            state: record.state.as_str().to_owned(),
            divergent: record.divergent,
            attention: record
                .attention
                .map(|attention| attention.as_str().to_owned())
                .unwrap_or_default(),
            created_at: Some(timestamp(record.created_at)),
            last_snapshot_at: record.last_snapshot_at.map(timestamp),
            last_snapshot_op: record.last_snapshot_op.clone().unwrap_or_default(),
            lease: view.lease,
            watcher: record.watcher.as_str().to_owned(),
            local_snapshot_count: view.local_snapshot_count,
            stores: view.stores.iter().map(v1::MountedStore::from).collect(),
            watch_state: view
                .watch_pending
                .map(PendingWatch::as_str)
                .unwrap_or_default()
                .to_owned(),
        }
    }
}

impl From<&MountedStore> for v1::MountedStore {
    fn from(store: &MountedStore) -> Self {
        let mode = match store.mode {
            MountMode::Symlink => v1::mounted_store::Mode::Symlink,
            MountMode::Bind => v1::mounted_store::Mode::Bind,
        };
        Self {
            name: store.key.name.clone(),
            lockfile_hash: store.key.lockfile_hash.clone(),
            mount: store.mount.clone(),
            mode: mode.into(),
            verified: store.verified,
        }
    }
}

impl From<&SnapshotEntry> for v1::SnapshotEntry {
    fn from(entry: &SnapshotEntry) -> Self {
        Self {
            op_id: entry.op_id.to_string(),
            commit_id: entry.commit_id.to_string(),
            kind: entry.kind.as_str().to_owned(),
            local: entry.local,
            taken_at: Some(timestamp(entry.taken_at)),
        }
    }
}

impl From<&DiskUsage> for v1::DiskUsage {
    fn from(usage: &DiskUsage) -> Self {
        Self {
            allocated_bytes: usage.allocated_bytes,
            files_changed: usage.files_changed,
        }
    }
}

impl From<&CreateTimings> for v1::CreateTimings {
    fn from(timings: &CreateTimings) -> Self {
        Self {
            resolve_ms: timings.resolve_ms,
            base_ms: timings.base_ms,
            materialize_ms: timings.materialize_ms,
            mount_ms: timings.mount_ms,
            jj_ms: timings.jj_ms,
            seed_ms: timings.seed_ms,
            insert_ms: timings.insert_ms,
            watch_ms: timings.watch_ms,
            total_ms: timings.total_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_split_into_seconds_and_nanos() {
        assert_eq!(
            proto_duration(Duration::from_millis(60_250)),
            prost_types::Duration {
                seconds: 60,
                nanos: 250_000_000
            }
        );
    }

    #[test]
    fn timestamps_split_into_seconds_and_nanos() {
        assert_eq!(
            timestamp(Timestamp(1_792_175_477_042)),
            prost_types::Timestamp {
                seconds: 1_792_175_477,
                nanos: 42_000_000
            }
        );
        // Before the epoch, nanos stay non-negative as protobuf requires.
        assert_eq!(
            timestamp(Timestamp(-1)),
            prost_types::Timestamp {
                seconds: -1,
                nanos: 999_000_000
            }
        );
    }
}
