//! Conversions between the daemon's records and the generated messages,
//! one per type, so the manager never sees the generated code. Model enums
//! become v1 enums here and nowhere else.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reef_api::v1;
use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::CommitIdHex;
use reef_lib::jj::diff::{ChangeKind, EntryKind, FileDiffInfo};
use reef_lib::jj::effects::{ChangeEffectKind, OpEffects};
use reef_lib::jj::log::{ChangeInfo, EdgeKind, LogNode, SignatureInfo};
use reef_lib::jj::oplog::{ATTR_AGENT, ATTR_SNAPSHOT_KIND, OpClass, OpEntry, OpKind};

use crate::backend::{BackendChoice, BackendKind, DiskUsage};
use crate::lease::{LeaseManager, LeaseState};
use crate::model::{
    Attention, LeaseEnd, LeaseRecord, RepoId, RepositoryRecord, Timestamp, WatcherKind,
    WorkspaceRecord, WorkspaceState,
};
use crate::snapshot::SnapshotKind;
use crate::snapshot::index::SnapshotEntry;
use crate::store::{MountMode, MountedStore};
use crate::watcher::PendingWatch;
use crate::workspace::{CreateTimings, MissingStores};

/// Unix milliseconds as a `google.protobuf.Timestamp`.
pub(super) fn timestamp(time: Timestamp) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: time.0.div_euclid(1000),
        // In 0..1000 ms, so the product fits in an i32.
        nanos: i32::try_from(time.0.rem_euclid(1000) * 1_000_000).unwrap_or(0),
    }
}

/// A [`SystemTime`] as a `google.protobuf.Timestamp`, to the millisecond.
pub(super) fn system_timestamp(time: SystemTime) -> prost_types::Timestamp {
    let millis = match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_millis()).unwrap_or(i64::MAX),
    };
    timestamp(Timestamp(millis))
}

/// A duration as a `google.protobuf.Duration`.
pub(super) fn proto_duration(duration: Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
        // Below one second, so it fits.
        nanos: i32::try_from(duration.subsec_nanos()).unwrap_or(0),
    }
}

/// A `google.protobuf.Duration` from a request: `None` when unset or zero,
/// or when negative.
pub(super) fn request_duration(duration: Option<&prost_types::Duration>) -> Option<Duration> {
    let duration = duration?;
    let secs = u64::try_from(duration.seconds).ok()?;
    let nanos = u32::try_from(duration.nanos).ok()?;
    Some(Duration::new(secs, nanos)).filter(|duration| !duration.is_zero())
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

fn lease_state(state: LeaseState) -> v1::LeaseState {
    match state {
        LeaseState::Active => v1::LeaseState::Active,
        LeaseState::InGrace => v1::LeaseState::InGrace,
        LeaseState::Expired => v1::LeaseState::Expired,
        LeaseState::Ended => v1::LeaseState::Ended,
    }
}

fn lease_end(end: Option<LeaseEnd>) -> v1::LeaseEndReason {
    match end {
        None => v1::LeaseEndReason::Unspecified,
        Some(LeaseEnd::Released) => v1::LeaseEndReason::Released,
        Some(LeaseEnd::Expired) => v1::LeaseEndReason::Expired,
        Some(LeaseEnd::Restored) => v1::LeaseEndReason::Restored,
    }
}

/// A lease as a message, its state computed at `now`.
pub(crate) fn lease(lease: &LeaseRecord, leases: &LeaseManager, now: Timestamp) -> v1::Lease {
    v1::Lease {
        id: lease.id.as_hex(),
        owner: lease.owner.clone(),
        issued_at: Some(timestamp(lease.issued_at)),
        expires_at: Some(timestamp(lease.expires_at)),
        last_heartbeat_at: Some(timestamp(lease.last_heartbeat_at)),
        length: Some(proto_duration(lease.length)),
        grace: Some(proto_duration(lease.grace)),
        state: lease_state(LeaseState::of(lease, now)).into(),
        end_reason: lease_end(lease.end_reason).into(),
        heartbeat_every: Some(proto_duration(leases.heartbeat_every(lease.length))),
    }
}

/// A backend as a message.
pub(super) fn backend(kind: BackendKind) -> v1::Backend {
    match kind {
        BackendKind::Copy => v1::Backend::Copy,
        BackendKind::ApfsClone => v1::Backend::Apfs,
        BackendKind::Reflink => v1::Backend::Reflink,
        BackendKind::Overlayfs => v1::Backend::Overlayfs,
        BackendKind::FuseOverlayfs => v1::Backend::FuseOverlayfs,
    }
}

/// A request's enum field holding a value this daemon does not know.
#[derive(Debug, thiserror::Error)]
#[error("{field} {value} is not a value this daemon knows")]
pub(super) struct UnknownValue {
    /// The request field.
    pub(super) field: &'static str,
    /// What it held.
    pub(super) value: i32,
}

impl Classify for UnknownValue {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// A request's enum field, refused when it holds a value this daemon does
/// not know (a newer client's), rather than read as unspecified.
pub(super) fn known<E: TryFrom<i32>>(field: &'static str, value: i32) -> Result<E, UnknownValue> {
    E::try_from(value).map_err(|_| UnknownValue { field, value })
}

/// A request's backend: `None` for the daemon's default.
pub(super) fn backend_choice(backend: v1::Backend) -> Option<BackendChoice> {
    let kind = match backend {
        v1::Backend::Unspecified => return None,
        v1::Backend::Auto => return Some(BackendChoice::Auto),
        v1::Backend::Copy => BackendKind::Copy,
        v1::Backend::Apfs => BackendKind::ApfsClone,
        v1::Backend::Reflink => BackendKind::Reflink,
        v1::Backend::Overlayfs => BackendKind::Overlayfs,
        v1::Backend::FuseOverlayfs => BackendKind::FuseOverlayfs,
    };
    Some(BackendChoice::Named(kind))
}

/// A request's missing-stores policy; unspecified is `Fail`.
pub(super) fn missing_stores(value: v1::MissingStores) -> MissingStores {
    match value {
        v1::MissingStores::Skip => MissingStores::Skip,
        v1::MissingStores::Fail | v1::MissingStores::Unspecified => MissingStores::Fail,
    }
}

/// A workspace state as a message.
fn workspace_state(state: WorkspaceState) -> v1::WorkspaceState {
    match state {
        WorkspaceState::Active => v1::WorkspaceState::Active,
        WorkspaceState::Releasing => v1::WorkspaceState::Releasing,
        WorkspaceState::Released => v1::WorkspaceState::Released,
        WorkspaceState::Reclaimed => v1::WorkspaceState::Reclaimed,
    }
}

/// A requested state; `None` for unspecified.
pub(super) fn state_of(state: v1::WorkspaceState) -> Option<WorkspaceState> {
    match state {
        v1::WorkspaceState::Unspecified => None,
        v1::WorkspaceState::Active => Some(WorkspaceState::Active),
        v1::WorkspaceState::Releasing => Some(WorkspaceState::Releasing),
        v1::WorkspaceState::Released => Some(WorkspaceState::Released),
        v1::WorkspaceState::Reclaimed => Some(WorkspaceState::Reclaimed),
    }
}

fn attention(attention: Option<Attention>) -> v1::Attention {
    match attention {
        None => v1::Attention::Unspecified,
        Some(Attention::Divergent) => v1::Attention::Divergent,
        Some(Attention::StaleRecovered) => v1::Attention::StaleRecovered,
        Some(Attention::FinalSnapshotFailed) => v1::Attention::FinalSnapshotFailed,
        Some(Attention::StoreHashMismatch) => v1::Attention::StoreHashMismatch,
        Some(Attention::StoreMissing) => v1::Attention::StoreMissing,
        Some(Attention::WatchFailed) => v1::Attention::WatchFailed,
        Some(Attention::UpperMissing) => v1::Attention::UpperMissing,
        Some(Attention::MountFailed) => v1::Attention::MountFailed,
    }
}

fn watcher(kind: WatcherKind) -> v1::WatcherKind {
    match kind {
        WatcherKind::Notify => v1::WatcherKind::Notify,
        WatcherKind::Watchman => v1::WatcherKind::Watchman,
    }
}

/// A snapshot kind as a message.
pub(super) fn snapshot_kind(kind: SnapshotKind) -> v1::SnapshotKind {
    match kind {
        SnapshotKind::Watcher => v1::SnapshotKind::Watcher,
        SnapshotKind::Explicit => v1::SnapshotKind::Explicit,
        SnapshotKind::Final => v1::SnapshotKind::Final,
        SnapshotKind::Move => v1::SnapshotKind::Move,
        SnapshotKind::Shutdown => v1::SnapshotKind::Shutdown,
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
    /// Whether the workspace is loaded: an active, loaded workspace with no
    /// pending watch reports its watch as attached.
    pub(super) loaded: bool,
}

impl From<WorkspaceView<'_>> for v1::Workspace {
    fn from(view: WorkspaceView<'_>) -> Self {
        let record = view.record;
        let watch_state = match view.watch_pending {
            Some(PendingWatch::Waiting) => v1::WatchState::Waiting,
            Some(PendingWatch::Attaching) => v1::WatchState::Attaching,
            None if view.loaded && record.state == WorkspaceState::Active => {
                v1::WatchState::Attached
            }
            None => v1::WatchState::Unspecified,
        };
        Self {
            id: record.id.as_hex(),
            repo_id: record.repo_id.as_hex(),
            owner: record.owner.clone(),
            purpose: record.purpose.clone(),
            change_id: record.change_id.clone(),
            commit_id: record.commit_id.clone(),
            backend: record
                .backend
                .parse::<BackendKind>()
                .map_or(v1::Backend::Unspecified, backend)
                .into(),
            path: record.path.display().to_string(),
            state: workspace_state(record.state).into(),
            divergent: record.divergent,
            attention: attention(record.attention).into(),
            created_at: Some(timestamp(record.created_at)),
            last_snapshot_at: record.last_snapshot_at.map(timestamp),
            last_snapshot_operation_id: record.last_snapshot_op.clone().unwrap_or_default(),
            lease: view.lease,
            watcher: watcher(record.watcher).into(),
            local_snapshot_count: view.local_snapshot_count,
            stores: view.stores.iter().map(v1::MountedStore::from).collect(),
            watch_state: watch_state.into(),
        }
    }
}

impl From<&MountedStore> for v1::MountedStore {
    fn from(store: &MountedStore) -> Self {
        let mode = match store.mode {
            MountMode::Symlink => v1::StoreMountMode::Symlink,
            MountMode::Bind => v1::StoreMountMode::Bind,
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
            operation_id: entry.op_id.to_string(),
            commit_id: entry.commit_id.to_string(),
            kind: snapshot_kind(entry.kind).into(),
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

/// Prefix of the jj workspace names of Reef workspaces.
const REEF_WORKSPACE: &str = "reef-";

/// What the RPC that just wrote an operation knows about it: its kind, the
/// workspace it was for, and its agent (the request's `reef-agent`, else
/// the workspace's owner), which are the attributes it wrote (R2.03).
#[derive(Clone, Copy, Debug)]
pub(super) struct Written<'a> {
    pub(super) kind: v1::OperationKind,
    pub(super) snapshot: Option<SnapshotKind>,
    /// The Reef workspace it was for, when the RPC names one. Only
    /// snapshots record a jj workspace name; creates, moves, releases and
    /// the write RPCs record none, and `reef.workspace` names it.
    pub(super) workspace: Option<&'a str>,
    pub(super) agent: &'a str,
}

/// An operation the daemon just wrote, as a message. Effects are left unset
/// (R2.02 computes them).
pub(super) fn operation(entry: &OpEntry, repo: RepoId, written: Written<'_>) -> v1::Operation {
    let (named, jj_workspace) = match entry.workspace.as_deref() {
        Some(name) => match name.strip_prefix(REEF_WORKSPACE) {
            Some(id) if !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()) => {
                (id.to_owned(), String::new())
            }
            _ => (String::new(), name.to_owned()),
        },
        None => (String::new(), String::new()),
    };
    let workspace_id = written.workspace.map_or(named, str::to_owned);
    let agent = entry
        .attributes
        .get(ATTR_AGENT)
        .map_or(written.agent, String::as_str);
    v1::Operation {
        id: entry.id.to_string(),
        repo_id: repo.as_hex(),
        parent_ids: entry.parents.iter().map(ToString::to_string).collect(),
        description: entry.description.clone(),
        started_at: Some(system_timestamp(entry.start)),
        ended_at: Some(system_timestamp(entry.end)),
        hostname: entry.hostname.clone(),
        username: entry.username.clone(),
        is_snapshot: entry.is_snapshot,
        kind: written.kind.into(),
        snapshot_kind: written
            .snapshot
            .map_or(v1::SnapshotKind::Unspecified, snapshot_kind)
            .into(),
        workspace_id,
        jj_workspace,
        agent: agent.to_owned(),
        attributes: entry.attributes.clone(),
        effects: None,
    }
}

/// An operation the daemon just wrote but could not read back, as a
/// message: what the RPC that wrote it knows. Effects are unset.
pub(super) fn unread_operation(id: &str, repo: RepoId, written: Written<'_>) -> v1::Operation {
    v1::Operation {
        id: id.to_owned(),
        repo_id: repo.as_hex(),
        kind: written.kind.into(),
        snapshot_kind: written
            .snapshot
            .map_or(v1::SnapshotKind::Unspecified, snapshot_kind)
            .into(),
        is_snapshot: written.kind == v1::OperationKind::Snapshot,
        workspace_id: written.workspace.unwrap_or_default().to_owned(),
        agent: written.agent.to_owned(),
        ..v1::Operation::default()
    }
}

/// A change as a message: Reef workspaces' working copies by id, others by
/// jj name.
pub(super) fn change(info: &ChangeInfo) -> v1::Change {
    let (workspace_ids, jj_workspaces) = split_workspaces(&info.workspaces);
    v1::Change {
        change_id: info.change_id.to_string(),
        commit_id: info.commit_id.to_string(),
        parent_commit_ids: info
            .parent_commit_ids
            .iter()
            .map(ToString::to_string)
            .collect(),
        parent_change_ids: info
            .parent_change_ids
            .iter()
            .map(ToString::to_string)
            .collect(),
        description: info.description.clone(),
        author: Some(signature(&info.author)),
        committer: Some(signature(&info.committer)),
        tree_id: info.tree_id.clone().unwrap_or_default(),
        empty: info.empty,
        conflict: info.conflict,
        divergent: info.divergent,
        immutable: info.immutable,
        hidden: info.hidden,
        bookmarks: info.bookmarks.clone(),
        tags: info.tags.clone(),
        workspace_ids,
        jj_workspaces,
    }
}

fn signature(signature: &SignatureInfo) -> v1::Signature {
    v1::Signature {
        name: signature.name.clone(),
        email: signature.email.clone(),
        at: Some(system_timestamp(signature.at)),
    }
}

/// The Reef workspace id in a jj workspace name `reef-<id>`.
fn reef_workspace(name: &str) -> Option<&str> {
    name.strip_prefix(REEF_WORKSPACE)
        .filter(|id| !id.is_empty() && id.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// jj workspace names as (Reef workspace ids, other jj names).
fn split_workspaces(names: &[String]) -> (Vec<String>, Vec<String>) {
    let mut reef = Vec::new();
    let mut other = Vec::new();
    for name in names {
        match reef_workspace(name) {
            Some(id) => reef.push(id.to_owned()),
            None => other.push(name.clone()),
        }
    }
    (reef, other)
}

/// A log entry as a message.
pub(super) fn log_entry(info: &ChangeInfo, node: &LogNode) -> v1::LogEntry {
    v1::LogEntry {
        change: Some(change(info)),
        edges: node
            .edges
            .iter()
            .map(|(commit, kind)| v1::GraphEdge {
                commit_id: commit.to_string(),
                kind: match kind {
                    EdgeKind::Direct => v1::GraphEdgeKind::Direct,
                    EdgeKind::Indirect => v1::GraphEdgeKind::Indirect,
                    EdgeKind::Missing => v1::GraphEdgeKind::Missing,
                }
                .into(),
            })
            .collect(),
    }
}

pub(super) fn entry_type(kind: Option<EntryKind>) -> v1::EntryType {
    match kind {
        None => v1::EntryType::Unspecified,
        Some(EntryKind::File) => v1::EntryType::File,
        Some(EntryKind::Executable) => v1::EntryType::Executable,
        Some(EntryKind::Symlink) => v1::EntryType::Symlink,
        Some(EntryKind::Directory) => v1::EntryType::Directory,
        Some(EntryKind::Conflict) => v1::EntryType::Conflict,
        Some(EntryKind::Submodule) => v1::EntryType::Submodule,
    }
}

/// A file's path and how it differs, as a message.
pub(super) fn file_change(diff: &FileDiffInfo) -> v1::FileChange {
    let kind = match diff.kind {
        None => v1::FileChangeKind::Unspecified,
        Some(ChangeKind::Added) => v1::FileChangeKind::Added,
        Some(ChangeKind::Modified) => v1::FileChangeKind::Modified,
        Some(ChangeKind::Deleted) => v1::FileChangeKind::Deleted,
        Some(ChangeKind::Renamed) => v1::FileChangeKind::Renamed,
        Some(ChangeKind::Copied) => v1::FileChangeKind::Copied,
    };
    v1::FileChange {
        path: diff.path.clone(),
        kind: kind.into(),
        old_path: diff.old_path.clone().unwrap_or_default(),
        old_type: entry_type(diff.old_type).into(),
        new_type: entry_type(diff.new_type).into(),
    }
}

/// A file's diff as a message.
pub(super) fn file_diff(diff: &FileDiffInfo) -> v1::FileDiff {
    v1::FileDiff {
        change: Some(file_change(diff)),
        binary: diff.binary,
        conflict: diff.conflict,
        lines_added: diff.lines_added,
        lines_removed: diff.lines_removed,
        hunks: diff
            .hunks
            .iter()
            .map(|hunk| v1::Hunk {
                old_start: hunk.old_start,
                old_lines: hunk.old_lines,
                new_start: hunk.new_start,
                new_lines: hunk.new_lines,
                lines: hunk.lines.clone(),
            })
            .collect(),
        truncated: diff.truncated,
        old_no_final_newline: diff.old_no_final_newline,
        new_no_final_newline: diff.new_no_final_newline,
        too_large: diff.too_large,
        invalid_utf8: diff.invalid_utf8,
    }
}

fn hex_or_empty(id: Option<&CommitIdHex>) -> String {
    id.map(ToString::to_string).unwrap_or_default()
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// An operation's effects as a message.
pub(crate) fn effects(effects: &OpEffects) -> v1::OperationEffects {
    v1::OperationEffects {
        changes: effects
            .changes
            .iter()
            .map(|change| v1::ChangeEffect {
                change_id: change.change_id.to_string(),
                kind: match change.kind {
                    ChangeEffectKind::Created => v1::ChangeEffectKind::Created,
                    ChangeEffectKind::Rewritten => v1::ChangeEffectKind::Rewritten,
                    ChangeEffectKind::Abandoned => v1::ChangeEffectKind::Abandoned,
                    ChangeEffectKind::Hidden => v1::ChangeEffectKind::Hidden,
                }
                .into(),
                commit_id: hex_or_empty(change.commit_id.as_ref()),
                previous_commit_id: hex_or_empty(change.previous_commit_id.as_ref()),
            })
            .collect(),
        bookmarks: effects
            .bookmarks
            .iter()
            .map(|bookmark| v1::BookmarkEffect {
                name: bookmark.name.clone(),
                remote: bookmark.remote.clone().unwrap_or_default(),
                commit_id: hex_or_empty(bookmark.commit_id.as_ref()),
                previous_commit_id: hex_or_empty(bookmark.previous_commit_id.as_ref()),
                conflicted: bookmark.conflicted,
            })
            .collect(),
        working_copies: effects
            .working_copies
            .iter()
            .map(|moved| {
                let (workspace_id, jj_workspace) = match reef_workspace(&moved.workspace) {
                    Some(id) => (id.to_owned(), String::new()),
                    None => (String::new(), moved.workspace.clone()),
                };
                v1::WorkingCopyEffect {
                    workspace_id,
                    jj_workspace,
                    commit_id: hex_or_empty(moved.commit_id.as_ref()),
                    previous_commit_id: hex_or_empty(moved.previous_commit_id.as_ref()),
                }
            })
            .collect(),
        change_count: to_u32(effects.change_count),
        bookmark_count: to_u32(effects.bookmark_count),
    }
}

/// A classification as the v1 kind.
pub(super) fn operation_kind(kind: Option<OpKind>) -> v1::OperationKind {
    match kind {
        None => v1::OperationKind::Unspecified,
        Some(OpKind::Snapshot) => v1::OperationKind::Snapshot,
        Some(OpKind::CreateWorkspace) => v1::OperationKind::CreateWorkspace,
        Some(OpKind::ReleaseWorkspace) => v1::OperationKind::ReleaseWorkspace,
        Some(OpKind::MoveWorkspace) => v1::OperationKind::MoveWorkspace,
        Some(OpKind::Describe) => v1::OperationKind::Describe,
        Some(OpKind::Abandon) => v1::OperationKind::Abandon,
        Some(OpKind::Undo) => v1::OperationKind::Undo,
        Some(OpKind::Edit) => v1::OperationKind::Edit,
        Some(OpKind::ImportRefs) => v1::OperationKind::ImportRefs,
        Some(OpKind::External) => v1::OperationKind::External,
        Some(OpKind::Other) => v1::OperationKind::Other,
    }
}

/// What the daemon's records add to an operation it did not just write.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct Recorded<'a> {
    /// The snapshot index's kind for it, when it has a row.
    pub(super) indexed: Option<SnapshotKind>,
    /// The owner of the workspace it was for, in any state.
    pub(super) owner: Option<&'a str>,
}

/// An operation the daemon did not just write, classified (R2.02): its
/// kind from `reef.kind` or Reef's descriptions, its snapshot kind from
/// `reef.snapshot-kind`, else the snapshot index, else the description.
pub(super) fn listed_operation(
    entry: &OpEntry,
    repo: RepoId,
    class: &OpClass,
    recorded: Recorded<'_>,
) -> v1::Operation {
    let jj_workspace = entry
        .workspace
        .as_deref()
        .filter(|name| reef_workspace(name).is_none())
        .unwrap_or_default()
        .to_owned();
    let snapshot = if class.kind == Some(OpKind::Snapshot) {
        let attribute = entry
            .attributes
            .get(ATTR_SNAPSHOT_KIND)
            .and_then(|kind| kind.parse::<SnapshotKind>().ok());
        let described = class
            .snapshot_kind
            .as_deref()
            .and_then(|kind| kind.parse::<SnapshotKind>().ok());
        attribute.or(recorded.indexed).or(described)
    } else {
        None
    };
    let agent = entry
        .attributes
        .get(ATTR_AGENT)
        .map_or(recorded.owner.unwrap_or_default(), String::as_str);
    v1::Operation {
        id: entry.id.to_string(),
        repo_id: repo.as_hex(),
        parent_ids: entry.parents.iter().map(ToString::to_string).collect(),
        description: entry.description.clone(),
        started_at: Some(system_timestamp(entry.start)),
        ended_at: Some(system_timestamp(entry.end)),
        hostname: entry.hostname.clone(),
        username: entry.username.clone(),
        is_snapshot: entry.is_snapshot,
        kind: operation_kind(class.kind).into(),
        snapshot_kind: snapshot
            .map_or(v1::SnapshotKind::Unspecified, snapshot_kind)
            .into(),
        workspace_id: class.workspace_id.clone().unwrap_or_default(),
        jj_workspace,
        agent: agent.to_owned(),
        attributes: entry.attributes.clone(),
        effects: None,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    /// R2.05 test 13: the thin client writes the daemon's names and
    /// timestamps out instead of borrowing them; they stay equal.
    #[test]
    fn json_names_match_the_daemon() {
        use reef_client::json::{attention_name, backend_name, rfc3339, snapshot_kind_name};
        for kind in [
            BackendKind::Copy,
            BackendKind::ApfsClone,
            BackendKind::Reflink,
            BackendKind::Overlayfs,
            BackendKind::FuseOverlayfs,
        ] {
            assert_eq!(backend_name(backend(kind).into()), kind.as_str());
        }
        for reason in Attention::ALL {
            assert_eq!(
                attention_name(attention(Some(*reason)).into()),
                Some(reason.as_str())
            );
        }
        assert_eq!(attention_name(attention(None).into()), None);
        for kind in SnapshotKind::ALL {
            assert_eq!(
                snapshot_kind_name(snapshot_kind(*kind).into()),
                kind.as_str()
            );
        }
        for millis in [
            0,
            1,
            -1,
            -86_400_000_123,
            1_700_000_000_123,
            253_402_300_799_999,
            i64::MAX / 1_000_000,
        ] {
            assert_eq!(
                rfc3339(&timestamp(Timestamp(millis))),
                Timestamp(millis).to_rfc3339(),
                "{millis}"
            );
        }
    }

    #[test]
    fn durations_split_into_seconds_and_nanos() {
        assert_eq!(
            proto_duration(Duration::from_millis(60_250)),
            prost_types::Duration {
                seconds: 60,
                nanos: 250_000_000
            }
        );
        assert_eq!(
            request_duration(Some(&proto_duration(Duration::from_millis(60_250)))),
            Some(Duration::from_millis(60_250))
        );
        assert_eq!(
            request_duration(Some(&prost_types::Duration::default())),
            None
        );
        let negative = prost_types::Duration {
            seconds: -1,
            nanos: 0,
        };
        assert_eq!(request_duration(Some(&negative)), None);
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

    #[test]
    fn every_backend_round_trips() {
        for kind in BackendKind::AUTO_ORDER {
            assert_eq!(
                backend_choice(backend(kind)),
                Some(BackendChoice::Named(kind))
            );
        }
        assert_eq!(backend_choice(v1::Backend::Unspecified), None);
        assert_eq!(backend_choice(v1::Backend::Auto), Some(BackendChoice::Auto));
    }

    #[test]
    fn operations_name_their_workspace() {
        let entry = |workspace: &str, attributes: BTreeMap<String, String>| OpEntry {
            id: "00ff".parse().unwrap(),
            parents: vec!["00aa".parse().unwrap()],
            description: "reef: create workspace 0123456789abcdef".into(),
            start: UNIX_EPOCH,
            end: UNIX_EPOCH + Duration::from_millis(1500),
            hostname: "host".into(),
            username: "user".into(),
            is_snapshot: false,
            workspace: Some(workspace.to_owned()),
            attributes,
        };
        let repo: RepoId = "aaaa000000000001".parse().unwrap();
        let written = Written {
            kind: v1::OperationKind::CreateWorkspace,
            snapshot: None,
            workspace: None,
            agent: "owner@host",
        };
        let op = operation(
            &entry("reef-0123456789abcdef", BTreeMap::new()),
            repo,
            written,
        );
        assert_eq!(op.workspace_id, "0123456789abcdef");
        assert_eq!(op.jj_workspace, "");
        assert_eq!(op.agent, "owner@host");
        assert_eq!(op.parent_ids, ["00aa"]);
        assert_eq!(op.kind(), v1::OperationKind::CreateWorkspace);
        assert_eq!(op.ended_at.unwrap().nanos, 500_000_000);
        let tagged = [("reef.agent".to_owned(), "agent-7".to_owned())].into();
        let op = operation(&entry("default", tagged), repo, written);
        assert_eq!(
            (op.workspace_id.as_str(), op.jj_workspace.as_str()),
            ("", "default")
        );
        assert_eq!(op.agent, "agent-7");
        // A create runs in `default`; the RPC knows its workspace.
        let written = Written {
            workspace: Some("0123456789abcdef"),
            ..written
        };
        let op = operation(&entry("default", BTreeMap::new()), repo, written);
        assert_eq!(
            (op.workspace_id.as_str(), op.jj_workspace.as_str()),
            ("0123456789abcdef", "default")
        );
    }
}
