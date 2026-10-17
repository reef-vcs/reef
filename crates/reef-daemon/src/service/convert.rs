//! Conversions between the daemon's records and the generated messages,
//! one per type, so the manager never sees the generated code.

use crate::backend::DiskUsage;
use crate::model::{LeaseRecord, RepositoryRecord, Timestamp, WorkspaceRecord};
use crate::proto::v0;

/// Unix milliseconds as a `google.protobuf.Timestamp`.
pub(super) fn timestamp(time: Timestamp) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: time.0.div_euclid(1000),
        // In 0..1000 ms, so the product fits in an i32.
        nanos: i32::try_from(time.0.rem_euclid(1000) * 1_000_000).unwrap_or(0),
    }
}

fn millis(duration: std::time::Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

impl From<&RepositoryRecord> for v0::Repository {
    fn from(record: &RepositoryRecord) -> Self {
        Self {
            id: record.id.as_hex(),
            root: record.root.display().to_string(),
            colocated: record.colocated,
            registered_at: Some(timestamp(record.registered_at)),
        }
    }
}

impl From<&LeaseRecord> for v0::Lease {
    fn from(lease: &LeaseRecord) -> Self {
        Self {
            id: lease.id.as_hex(),
            owner: lease.owner.clone(),
            issued_at: Some(timestamp(lease.issued_at)),
            expires_at: Some(timestamp(lease.expires_at)),
            last_heartbeat_at: Some(timestamp(lease.last_heartbeat_at)),
            length_ms: millis(lease.length),
            grace_ms: millis(lease.grace),
        }
    }
}

/// A workspace row and its unended lease, as one message.
pub(super) struct WorkspaceView<'a> {
    pub(super) record: &'a WorkspaceRecord,
    pub(super) lease: Option<&'a LeaseRecord>,
}

impl From<WorkspaceView<'_>> for v0::Workspace {
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
            lease: view.lease.map(v0::Lease::from),
        }
    }
}

impl From<&DiskUsage> for v0::DiskUsage {
    fn from(usage: &DiskUsage) -> Self {
        Self {
            allocated_bytes: usage.allocated_bytes,
            files_changed: usage.files_changed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
