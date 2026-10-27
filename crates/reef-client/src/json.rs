//! The JSON form of the API's responses, as `reef -o json` prints them
//! (R1.22): serde structs built from the generated messages, so the field
//! names are fixed `snake_case` whatever the generated code calls them,
//! timestamps are RFC 3339 UTC strings, ids are full lowercase hex, and an
//! absent value (an empty string or unset field in the message) is `null`.
//!
//! The v1 enums are printed as the strings Phase 1 printed (`apfs`,
//! `fuse-overlayfs`, `stale-recovered`), so the JSON did not change with
//! the API (R2.01); a value this build does not know prints as `unknown`.
//!
//! The VCS commands' types (R2.27) follow the same rules: keys are the v1
//! field names, every object with a `change_id` also has
//! `change_id_reverse_hex`, and a string the API leaves empty to mean
//! "none" is `null` while free text such as a description stays a string.
//!
//! These are output types; nothing parses them back, except [`ErrorObject`]
//! and [`Restored`], which the `reef-daemon` binary's offline commands
//! print for `reef` to read (R2.05).

use std::collections::BTreeMap;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reef_api::v1;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// A `google.protobuf.Timestamp` as RFC 3339 with millisecond precision,
/// as the daemon's own timestamps print (a test in the daemon keeps the two
/// equal); `<millis>ms` for an instant `time` cannot represent.
pub fn rfc3339(time: &prost_types::Timestamp) -> String {
    let millis = millis_of(time);
    OffsetDateTime::from_unix_timestamp_nanos(i128::from(millis) * 1_000_000)
        .ok()
        .and_then(|time| time.format(&Rfc3339).ok())
        .unwrap_or_else(|| format!("{millis}ms"))
}

/// A `google.protobuf.Timestamp` as Unix milliseconds.
pub fn millis_of(time: &prost_types::Timestamp) -> i64 {
    time.seconds
        .saturating_mul(1000)
        .saturating_add(i64::from(time.nanos / 1_000_000))
}

fn time(value: Option<&prost_types::Timestamp>) -> Option<String> {
    value.map(rfc3339)
}

fn text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

/// What an enum value this build does not know prints as.
pub const UNKNOWN: &str = "unknown";

/// A backend's name, as `workspaces.backend` stores it (the daemon's
/// `BackendKind::as_str`; a test in the daemon keeps the two equal).
pub fn backend_name(backend: i32) -> &'static str {
    match v1::Backend::try_from(backend) {
        Ok(v1::Backend::Copy) => "copy",
        Ok(v1::Backend::Apfs) => "apfs",
        Ok(v1::Backend::Reflink) => "reflink",
        Ok(v1::Backend::Overlayfs) => "overlayfs",
        Ok(v1::Backend::FuseOverlayfs) => "fuse-overlayfs",
        Ok(v1::Backend::Auto) => "auto",
        Ok(v1::Backend::Unspecified) | Err(_) => UNKNOWN,
    }
}

/// A workspace state's name: `active`, `releasing`, `released`, or
/// `reclaimed`.
pub fn state_name(state: i32) -> &'static str {
    match v1::WorkspaceState::try_from(state) {
        Ok(v1::WorkspaceState::Active) => "active",
        Ok(v1::WorkspaceState::Releasing) => "releasing",
        Ok(v1::WorkspaceState::Released) => "released",
        Ok(v1::WorkspaceState::Reclaimed) => "reclaimed",
        Ok(v1::WorkspaceState::Unspecified) | Err(_) => UNKNOWN,
    }
}

/// An attention reason's name, such as `final-snapshot-failed`; `None`
/// when the workspace needs none. The daemon's `Attention::as_str`; a test
/// in the daemon keeps the two equal.
pub fn attention_name(attention: i32) -> Option<&'static str> {
    let name = match v1::Attention::try_from(attention) {
        Ok(v1::Attention::Unspecified) => return None,
        Ok(v1::Attention::Divergent) => "divergent",
        Ok(v1::Attention::StaleRecovered) => "stale-recovered",
        Ok(v1::Attention::FinalSnapshotFailed) => "final-snapshot-failed",
        Ok(v1::Attention::StoreHashMismatch) => "store-hash-mismatch",
        Ok(v1::Attention::StoreMissing) => "store-missing",
        Ok(v1::Attention::WatchFailed) => "watch-failed",
        Ok(v1::Attention::UpperMissing) => "upper-missing",
        Ok(v1::Attention::MountFailed) => "mount-failed",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

fn watcher_name(watcher: i32) -> &'static str {
    match v1::WatcherKind::try_from(watcher) {
        Ok(v1::WatcherKind::Notify) => "notify",
        Ok(v1::WatcherKind::Watchman) => "watchman",
        Ok(v1::WatcherKind::Unspecified) | Err(_) => UNKNOWN,
    }
}

/// Why a watch is not up yet: `waiting` or `attaching`; `None` once it is,
/// or when not reported.
fn watch_state_name(state: i32) -> Option<&'static str> {
    match v1::WatchState::try_from(state) {
        Ok(v1::WatchState::Waiting) => Some("waiting"),
        Ok(v1::WatchState::Attaching) => Some("attaching"),
        Ok(v1::WatchState::Attached | v1::WatchState::Unspecified) => None,
        Err(_) => Some(UNKNOWN),
    }
}

/// A snapshot kind's name: `watcher`, `explicit`, `final`, `move`, or
/// `shutdown`.
pub fn snapshot_kind_name(kind: i32) -> &'static str {
    match v1::SnapshotKind::try_from(kind) {
        Ok(v1::SnapshotKind::Watcher) => "watcher",
        Ok(v1::SnapshotKind::Explicit) => "explicit",
        Ok(v1::SnapshotKind::Final) => "final",
        Ok(v1::SnapshotKind::Move) => "move",
        Ok(v1::SnapshotKind::Shutdown) => "shutdown",
        Ok(v1::SnapshotKind::Unspecified) | Err(_) => UNKNOWN,
    }
}

fn operation_id(operation: Option<&v1::Operation>) -> Option<String> {
    operation.map(|operation| operation.id.clone())
}

/// A hex change id in jj's "reverse hex", the spelling `jj log` shows: the
/// digits `0` to `f` become the letters `z` to `k`. Other characters are
/// kept.
pub fn reverse_hex(hex: &str) -> String {
    hex.bytes()
        .map(|digit| match digit {
            b'0'..=b'9' => char::from(b'z' - (digit - b'0')),
            b'a'..=b'f' => char::from(b'p' - (digit - b'a')),
            other => char::from(other),
        })
        .collect()
}

/// A registered repository.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Repository {
    /// 16 hex characters.
    pub id: String,
    /// The root of the user's own jj workspace.
    pub root: String,
    /// Whether jj and git share the working copy.
    pub colocated: bool,
    /// When it was registered.
    pub registered_at: Option<String>,
}

impl From<&v1::Repository> for Repository {
    fn from(repo: &v1::Repository) -> Self {
        Self {
            id: repo.id.clone(),
            root: repo.root.clone(),
            colocated: repo.colocated,
            registered_at: time(repo.registered_at.as_ref()),
        }
    }
}

/// A workspace's lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Lease {
    /// The lease's id.
    pub id: String,
    /// Who holds it.
    pub owner: String,
    /// When it was issued.
    pub issued_at: Option<String>,
    /// When it lapses unless renewed.
    pub expires_at: Option<String>,
    /// Unix milliseconds of `expires_at`, for relative display.
    #[serde(skip)]
    pub expires_at_ms: Option<i64>,
    /// The last heartbeat.
    pub last_heartbeat_at: Option<String>,
    /// How far each heartbeat extends it.
    pub length_ms: u64,
    /// How long after `expires_at` a heartbeat still revives it.
    pub grace_ms: u64,
    /// `active`, `in_grace`, `expired`, or `ended`, as of the response.
    pub state: String,
    /// `released`, `expired`, or `restored` once it has ended.
    pub end_reason: Option<String>,
    /// How often its holder should heartbeat.
    pub heartbeat_every_ms: Option<u64>,
}

/// A lease state's JSON name.
fn lease_state(state: i32) -> &'static str {
    match v1::LeaseState::try_from(state) {
        Ok(v1::LeaseState::Active) => "active",
        Ok(v1::LeaseState::InGrace) => "in_grace",
        Ok(v1::LeaseState::Expired) => "expired",
        Ok(v1::LeaseState::Ended) => "ended",
        Ok(v1::LeaseState::Unspecified) | Err(_) => UNKNOWN,
    }
}

/// Why a lease ended: `released`, `expired`, or `restored`; `None` until it
/// does.
fn lease_end_reason(reason: i32) -> Option<&'static str> {
    match v1::LeaseEndReason::try_from(reason) {
        Ok(v1::LeaseEndReason::Released) => Some("released"),
        Ok(v1::LeaseEndReason::Expired) => Some("expired"),
        Ok(v1::LeaseEndReason::Restored) => Some("restored"),
        Ok(v1::LeaseEndReason::Unspecified) => None,
        Err(_) => Some(UNKNOWN),
    }
}

/// A `google.protobuf.Duration` in whole milliseconds; negative is zero.
pub fn duration_millis(duration: &prost_types::Duration) -> u64 {
    let millis = duration
        .seconds
        .saturating_mul(1000)
        .saturating_add(i64::from(duration.nanos / 1_000_000));
    u64::try_from(millis).unwrap_or(0)
}

impl From<&v1::Lease> for Lease {
    fn from(lease: &v1::Lease) -> Self {
        Self {
            id: lease.id.clone(),
            owner: lease.owner.clone(),
            issued_at: time(lease.issued_at.as_ref()),
            expires_at: time(lease.expires_at.as_ref()),
            expires_at_ms: lease.expires_at.as_ref().map(millis_of),
            last_heartbeat_at: time(lease.last_heartbeat_at.as_ref()),
            length_ms: lease.length.as_ref().map_or(0, duration_millis),
            grace_ms: lease.grace.as_ref().map_or(0, duration_millis),
            state: lease_state(lease.state).to_owned(),
            end_reason: lease_end_reason(lease.end_reason).map(str::to_owned),
            heartbeat_every_ms: lease.heartbeat_every.as_ref().map(duration_millis),
        }
    }
}

/// A Reef workspace.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Workspace {
    /// 16 hex characters.
    pub id: String,
    /// Its repository's id.
    pub repo_id: String,
    /// Who holds it.
    pub owner: String,
    /// Free text; empty if not given.
    pub purpose: String,
    /// The jj change it is on, in hex.
    pub change_id: String,
    /// The same change id in jj's reverse hex, as `jj log` shows it.
    pub change_id_reverse_hex: String,
    /// Its working-copy commit as of the last snapshot or check-out.
    pub commit_id: String,
    /// The materialization backend.
    pub backend: String,
    /// Its directory.
    pub path: String,
    /// `active`, `releasing`, `released`, or `reclaimed`.
    pub state: String,
    /// Whether its change is divergent.
    pub divergent: bool,
    /// Why it needs attention, if it does.
    pub attention: Option<String>,
    /// When it was created.
    pub created_at: Option<String>,
    /// When its last snapshot recorded an operation.
    pub last_snapshot_at: Option<String>,
    /// That operation.
    pub last_snapshot_op: Option<String>,
    /// Its lease: the unended one while active, the one it ended with once
    /// released or reclaimed.
    pub lease: Option<Lease>,
    /// What triggers its snapshots: `notify` or `watchman`.
    pub watcher: String,
    /// `waiting` while its Watchman subscription waits for a burst of
    /// creates to end (R1.29); `attaching` while its notify watch attaches
    /// in the background (Linux, R1.32).
    pub watch_state: Option<String>,
    /// How many of its snapshots have not been published; 0 in `ws create`.
    pub local_snapshot_count: u64,
    /// The shared stores mounted in it (R1.20).
    pub stores: Vec<MountedStore>,
}

/// A shared store mounted in a workspace (R1.20).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MountedStore {
    /// The store's name.
    pub name: String,
    /// Its lockfile's blake3 hash.
    pub lockfile_hash: String,
    /// Where, repository-root relative.
    pub mount: String,
    /// `symlink` or `bind`.
    pub mode: String,
    /// Whether the workspace's lockfile still matches the store.
    pub verified: bool,
}

impl From<&v1::MountedStore> for MountedStore {
    fn from(store: &v1::MountedStore) -> Self {
        let mode = match v1::StoreMountMode::try_from(store.mode) {
            Ok(v1::StoreMountMode::Symlink) => "symlink",
            Ok(v1::StoreMountMode::Bind) => "bind",
            Ok(v1::StoreMountMode::Unspecified) | Err(_) => UNKNOWN,
        };
        Self {
            name: store.name.clone(),
            lockfile_hash: store.lockfile_hash.clone(),
            mount: store.mount.clone(),
            mode: mode.to_owned(),
            verified: store.verified,
        }
    }
}

impl From<&v1::Workspace> for Workspace {
    fn from(ws: &v1::Workspace) -> Self {
        Self {
            id: ws.id.clone(),
            repo_id: ws.repo_id.clone(),
            owner: ws.owner.clone(),
            purpose: ws.purpose.clone(),
            change_id: ws.change_id.clone(),
            change_id_reverse_hex: reverse_hex(&ws.change_id),
            commit_id: ws.commit_id.clone(),
            backend: backend_name(ws.backend).to_owned(),
            path: ws.path.clone(),
            state: state_name(ws.state).to_owned(),
            divergent: ws.divergent,
            attention: attention_name(ws.attention).map(str::to_owned),
            created_at: time(ws.created_at.as_ref()),
            last_snapshot_at: time(ws.last_snapshot_at.as_ref()),
            last_snapshot_op: text(&ws.last_snapshot_operation_id),
            lease: ws.lease.as_ref().map(Lease::from),
            watcher: watcher_name(ws.watcher).to_owned(),
            watch_state: watch_state_name(ws.watch_state).map(str::to_owned),
            local_snapshot_count: ws.local_snapshot_count,
            stores: ws.stores.iter().map(MountedStore::from).collect(),
        }
    }
}

/// One indexed snapshot operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SnapshotEntry {
    /// The operation.
    pub op_id: String,
    /// The working-copy commit it wrote.
    pub commit_id: String,
    /// `watcher`, `explicit`, `final`, `move`, or `shutdown`.
    pub kind: String,
    /// Not yet published.
    pub local: bool,
    /// When the operation ended.
    pub taken_at: Option<String>,
}

impl From<&v1::SnapshotEntry> for SnapshotEntry {
    fn from(entry: &v1::SnapshotEntry) -> Self {
        Self {
            op_id: entry.operation_id.clone(),
            commit_id: entry.commit_id.clone(),
            kind: snapshot_kind_name(entry.kind).to_owned(),
            local: entry.local,
            taken_at: time(entry.taken_at.as_ref()),
        }
    }
}

/// How much disk a workspace uses beyond its base. An estimate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct DiskUsage {
    /// Allocated bytes of the files that differ from the base.
    pub allocated_bytes: u64,
    /// How many files differ from the base.
    pub files_changed: u64,
}

impl From<&v1::DiskUsage> for DiskUsage {
    fn from(usage: &v1::DiskUsage) -> Self {
        Self {
            allocated_bytes: usage.allocated_bytes,
            files_changed: usage.files_changed,
        }
    }
}

/// `WorkspaceService.CreateWorkspace`'s answer: the workspace's fields, then
/// the create's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Created {
    /// The new workspace.
    #[serde(flatten)]
    pub workspace: Workspace,
    /// The operation that created the jj workspace.
    pub op_id: Option<String>,
    /// Another Reef workspace was already editing the same commit.
    pub shared_change: bool,
    /// Wall time of making the directory.
    pub materialize_ms: u64,
    /// Whether the base cache already held the tree.
    pub base_was_cached: bool,
    /// How long each step took (R1.29).
    pub timings: CreateTimings,
    /// What the default start chose: a bookmark, or `root checkout`; `None`
    /// when the request named a start (R1.31).
    pub start: Option<String>,
}

impl Created {
    /// From the response; `None` if it carries no workspace.
    pub fn from_response(response: &v1::CreateWorkspaceResponse) -> Option<Self> {
        let timings = response.timings.unwrap_or_default();
        Some(Self {
            workspace: Workspace::from(response.workspace.as_ref()?),
            op_id: operation_id(response.operation.as_ref()),
            shared_change: response.shared_change,
            materialize_ms: timings.base_ms.saturating_add(timings.materialize_ms),
            base_was_cached: response.base_was_cached,
            timings: CreateTimings::from(&timings),
            start: text(&response.start),
        })
    }
}

/// A create's steps, in milliseconds; see `v1::CreateTimings`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CreateTimings {
    /// Resolving the start revision.
    pub resolve_ms: u64,
    /// Ensuring the base.
    pub base_ms: u64,
    /// The backend's materialization alone.
    pub materialize_ms: u64,
    /// Mounting the shared stores.
    pub mount_ms: u64,
    /// Creating the jj workspace.
    pub jj_ms: u64,
    /// Seeding the working copy's tree state.
    pub seed_ms: u64,
    /// Writing the row and issuing the lease.
    pub insert_ms: u64,
    /// Attaching the file watcher.
    pub watch_ms: u64,
    /// The whole create.
    pub total_ms: u64,
}

impl From<&v1::CreateTimings> for CreateTimings {
    fn from(timings: &v1::CreateTimings) -> Self {
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

/// `WorkspaceService.MoveWorkspace`'s answer (R1.06): the workspace's fields, then the
/// move's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Moved {
    /// The workspace afterwards.
    #[serde(flatten)]
    pub workspace: Workspace,
    /// The operation that moved the pointer, if it recorded one.
    pub op_id: Option<String>,
    /// The final snapshot of the change it left, if it recorded one.
    pub final_op_id: Option<String>,
    /// Files the check-out rewrote, wrote, and removed.
    pub files_updated: u32,
    /// See `files_updated`.
    pub files_added: u32,
    /// See `files_updated`.
    pub files_removed: u32,
    /// The backend replaced the whole tree instead (overlay backends).
    pub tree_replaced: bool,
}

impl Moved {
    /// From the response; `None` if it carries no workspace.
    pub fn from_response(response: &v1::MoveWorkspaceResponse) -> Option<Self> {
        Some(Self {
            workspace: Workspace::from(response.workspace.as_ref()?),
            op_id: operation_id(response.operation.as_ref()),
            final_op_id: operation_id(response.final_snapshot.as_ref()),
            files_updated: response.files_updated,
            files_added: response.files_added,
            files_removed: response.files_removed,
            tree_replaced: response.tree_replaced,
        })
    }
}

/// `WorkspaceService.GetWorkspace`'s answer: the workspace's fields, then
/// the rest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Inspected {
    /// The workspace, after the snapshot if one was taken.
    #[serde(flatten)]
    pub workspace: Workspace,
    /// `null` for a workspace whose directory is gone.
    pub disk_usage: Option<DiskUsage>,
    /// Ids of the other active workspaces on the same change.
    pub other_workspaces_on_change: Vec<String>,
    /// The operation the snapshot recorded, if it recorded one.
    pub op_id: Option<String>,
    /// Its indexed snapshot operations.
    pub snapshot_count: u64,
    /// The newest 10 of them, newest first.
    pub recent_snapshots: Vec<SnapshotEntry>,
}

impl Inspected {
    /// From the response; `None` if it carries no workspace.
    pub fn from_response(response: &v1::GetWorkspaceResponse) -> Option<Self> {
        Some(Self {
            workspace: Workspace::from(response.workspace.as_ref()?),
            disk_usage: response.disk_usage.as_ref().map(DiskUsage::from),
            other_workspaces_on_change: response.other_workspaces_on_change.clone(),
            op_id: operation_id(response.snapshot.as_ref()),
            snapshot_count: response.snapshot_count,
            recent_snapshots: response
                .recent_snapshots
                .iter()
                .map(SnapshotEntry::from)
                .collect(),
        })
    }
}

/// `WorkspaceService.ReleaseWorkspace`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Released {
    /// The workspace released.
    pub id: String,
    /// The operation holding the final snapshot, if it recorded one.
    pub final_op_id: Option<String>,
    /// Whether a final snapshot ran (it may have found no changes).
    pub snapshot_taken: bool,
    /// Bytes the workspace had allocated, estimated before tear-down.
    pub freed_bytes: u64,
}

impl From<&v1::ReleaseWorkspaceResponse> for Released {
    fn from(response: &v1::ReleaseWorkspaceResponse) -> Self {
        Self {
            id: response.workspace_id.clone(),
            final_op_id: operation_id(response.final_snapshot.as_ref()),
            snapshot_taken: response.snapshot_taken,
            freed_bytes: response.freed_bytes,
        }
    }
}

/// One materialization backend's probe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BackendProbe {
    /// The backend's name.
    pub kind: String,
    /// Whether it can serve workspace roots under the state directory.
    pub available: bool,
    /// Why it is unavailable.
    pub reason: Option<String>,
    /// The filesystem probed.
    pub filesystem: String,
}

impl From<&v1::BackendProbe> for BackendProbe {
    fn from(probe: &v1::BackendProbe) -> Self {
        Self {
            kind: backend_name(probe.backend).to_owned(),
            available: probe.available,
            reason: text(&probe.reason),
            filesystem: probe.filesystem.clone(),
        }
    }
}

/// `DaemonService.GetDaemonStatus`'s answer, or 0.1's `Daemon/Status`'s.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    /// The daemon's version.
    pub version: String,
    /// The API it serves: `v1`, or `v0` for a 0.1 daemon (R2.01).
    pub api: String,
    /// The revision of `reef.api.v1` it serves; `null` for a 0.1 daemon.
    pub api_revision: Option<u32>,
    /// The daemon's process id.
    pub pid: u32,
    /// Milliseconds since the daemon started.
    pub uptime_ms: u64,
    /// The socket the answer came over.
    pub socket: String,
    /// The state directory.
    pub state_dir: String,
    /// The metadata database's schema version.
    pub schema_version: u32,
    /// Registered repositories.
    pub repositories: u32,
    /// Workspaces in the `active` state.
    pub active_workspaces: u32,
    /// Each backend's probe.
    pub backends: Vec<BackendProbe>,
}

impl From<&v1::GetDaemonStatusResponse> for Status {
    fn from(status: &v1::GetDaemonStatusResponse) -> Self {
        Self {
            version: status.version.clone(),
            api: "v1".to_owned(),
            api_revision: Some(status.api_revision),
            pid: status.pid,
            uptime_ms: status.uptime_ms,
            socket: status.socket.clone(),
            state_dir: status.state_dir.clone(),
            schema_version: status.schema_version,
            repositories: status.repository_count,
            active_workspaces: status.active_workspace_count,
            backends: status.backends.iter().map(BackendProbe::from).collect(),
        }
    }
}

impl From<&reef_api::v0::StatusResponse> for Status {
    fn from(status: &reef_api::v0::StatusResponse) -> Self {
        Self {
            version: status.version.clone(),
            api: "v0".to_owned(),
            api_revision: None,
            pid: status.pid,
            uptime_ms: status.uptime_ms,
            socket: status.socket.clone(),
            state_dir: status.state_dir.clone(),
            schema_version: status.schema_version,
            repositories: status.repositories,
            active_workspaces: status.active_workspaces,
            backends: status
                .backends
                .iter()
                .map(|probe| BackendProbe {
                    kind: probe.kind.clone(),
                    available: probe.available,
                    reason: text(&probe.reason),
                    filesystem: probe.filesystem.clone(),
                })
                .collect(),
        }
    }
}

/// A built store, from `StoreService.ListStores` (R1.20).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Store {
    /// The `[[store]] name`.
    pub name: String,
    /// blake3 of its lockfile.
    pub lockfile_hash: String,
    /// Its directory.
    pub path: String,
    /// Allocated bytes, measured when listed.
    pub bytes: u64,
    /// When its build was sealed.
    pub built_at: Option<String>,
    /// Whether it is sealed.
    pub sealed: bool,
    /// Active workspaces that mount it.
    pub in_use_by: u32,
    /// The repository that built it first.
    pub repo_id: String,
}

impl From<&v1::StoreInfo> for Store {
    fn from(store: &v1::StoreInfo) -> Self {
        Self {
            name: store.name.clone(),
            lockfile_hash: store.lockfile_hash.clone(),
            path: store.path.clone(),
            bytes: store.bytes,
            built_at: time(store.built_at.as_ref()),
            sealed: store.sealed,
            in_use_by: store.in_use_by,
            repo_id: store.repo_id.clone(),
        }
    }
}

/// How a `StoreService.BuildStore` ended (R1.20).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BuiltStore {
    /// The store.
    pub name: String,
    /// blake3 of its lockfile.
    pub lockfile_hash: String,
    /// The sealed store's directory, or the failed build's.
    pub path: String,
    /// Allocated bytes of the sealed store.
    pub bytes: u64,
    /// Whether the store is sealed.
    pub sealed: bool,
    /// The build command's exit code; -1 for a signal.
    pub exit_code: i32,
    /// The operation that released the scratch workspace.
    pub op_id: Option<String>,
    /// Output lines dropped because the client read too slowly.
    pub dropped_lines: u64,
    /// A sealed store already existed; nothing was built.
    pub already_built: bool,
}

impl From<&v1::BuildResult> for BuiltStore {
    fn from(result: &v1::BuildResult) -> Self {
        Self {
            name: result.name.clone(),
            lockfile_hash: result.lockfile_hash.clone(),
            path: result.path.clone(),
            bytes: result.bytes,
            sealed: result.sealed,
            exit_code: result.exit_code,
            op_id: operation_id(result.release_operation.as_ref()),
            dropped_lines: result.dropped_lines,
            already_built: result.already_built,
        }
    }
}

/// One repository's report from `MaintenanceService.CollectGarbage`
/// (R1.17).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GcReport {
    /// The repository.
    pub repo_id: String,
    /// Nothing newer than this was deleted.
    pub keep_newer: Option<String>,
    /// The operation store and git objects before the jj step.
    pub jj_bytes_before: u64,
    /// The same, after.
    pub jj_bytes_after: u64,
    /// Rebuilding jj's index.
    pub reindex_ms: u64,
    /// Why the jj step stopped short.
    pub jj_error: Option<String>,
    /// Base cache entries removed.
    pub base_entries_removed: u32,
    /// What they had allocated.
    pub base_bytes_freed: u64,
    /// Ended workspace rows deleted.
    pub workspace_rows_removed: u32,
    /// What their state directories had allocated.
    pub wc_bytes_freed: u64,
    /// Stores removed.
    pub stores_removed: u32,
    /// What they had allocated.
    pub store_bytes_freed: u64,
    /// The run for this repository.
    pub elapsed_ms: u64,
    /// Nothing was deleted; the counts are what would have gone.
    pub dry_run: bool,
}

impl From<&v1::GcReport> for GcReport {
    fn from(report: &v1::GcReport) -> Self {
        Self {
            repo_id: report.repo_id.clone(),
            keep_newer: time(report.keep_newer.as_ref()),
            jj_bytes_before: report.jj_bytes_before,
            jj_bytes_after: report.jj_bytes_after,
            reindex_ms: report.reindex_ms,
            jj_error: text(&report.jj_error),
            base_entries_removed: report.base_entries_removed,
            base_bytes_freed: report.base_bytes_freed,
            workspace_rows_removed: report.workspace_rows_removed,
            wc_bytes_freed: report.wc_bytes_freed,
            stores_removed: report.stores_removed,
            store_bytes_freed: report.store_bytes_freed,
            elapsed_ms: report.elapsed_ms,
            dry_run: report.dry_run,
        }
    }
}

// The VCS services' messages (R2.27): what `reef status`, `log`, `show`,
// `diff`, `describe`, `abandon`, `undo`, `op`, `ws snapshot` and `file`
// print. Enums print as lowercase names with `-` between words, an unknown
// value as `unknown` and an unset one as `null`.

/// An entry type's name: `file`, `executable`, `symlink`, `directory`,
/// `conflict`, or `submodule`; `None` when unset.
pub fn entry_type_name(kind: i32) -> Option<&'static str> {
    let name = match v1::EntryType::try_from(kind) {
        Ok(v1::EntryType::Unspecified) => return None,
        Ok(v1::EntryType::File) => "file",
        Ok(v1::EntryType::Executable) => "executable",
        Ok(v1::EntryType::Symlink) => "symlink",
        Ok(v1::EntryType::Directory) => "directory",
        Ok(v1::EntryType::Conflict) => "conflict",
        Ok(v1::EntryType::Submodule) => "submodule",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// A file change's name: `added`, `modified`, `deleted`, `renamed`, or
/// `copied`; `None` when unset.
pub fn file_change_kind_name(kind: i32) -> Option<&'static str> {
    let name = match v1::FileChangeKind::try_from(kind) {
        Ok(v1::FileChangeKind::Unspecified) => return None,
        Ok(v1::FileChangeKind::Added) => "added",
        Ok(v1::FileChangeKind::Modified) => "modified",
        Ok(v1::FileChangeKind::Deleted) => "deleted",
        Ok(v1::FileChangeKind::Renamed) => "renamed",
        Ok(v1::FileChangeKind::Copied) => "copied",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// Where a status's changed files came from: `snapshot`, `index`, `walk`,
/// or `last-snapshot`; `None` when unset.
pub fn status_source_name(source: i32) -> Option<&'static str> {
    let name = match v1::StatusSource::try_from(source) {
        Ok(v1::StatusSource::Unspecified) => return None,
        Ok(v1::StatusSource::Snapshot) => "snapshot",
        Ok(v1::StatusSource::Index) => "index",
        Ok(v1::StatusSource::Walk) => "walk",
        Ok(v1::StatusSource::LastSnapshot) => "last-snapshot",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// A log graph edge's name: `direct`, `indirect`, or `missing`; `None`
/// when unset.
pub fn graph_edge_kind_name(kind: i32) -> Option<&'static str> {
    let name = match v1::GraphEdgeKind::try_from(kind) {
        Ok(v1::GraphEdgeKind::Unspecified) => return None,
        Ok(v1::GraphEdgeKind::Direct) => "direct",
        Ok(v1::GraphEdgeKind::Indirect) => "indirect",
        Ok(v1::GraphEdgeKind::Missing) => "missing",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// What an operation did to a change: `created`, `rewritten`,
/// `abandoned`, or `hidden`; `None` when unset.
pub fn change_effect_kind_name(kind: i32) -> Option<&'static str> {
    let name = match v1::ChangeEffectKind::try_from(kind) {
        Ok(v1::ChangeEffectKind::Unspecified) => return None,
        Ok(v1::ChangeEffectKind::Created) => "created",
        Ok(v1::ChangeEffectKind::Rewritten) => "rewritten",
        Ok(v1::ChangeEffectKind::Abandoned) => "abandoned",
        Ok(v1::ChangeEffectKind::Hidden) => "hidden",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// An operation kind's name, its `reef.kind` attribute (`snapshot`,
/// `create-workspace`, …; the daemon's `OpKind::as_str`); `None` when
/// unset.
pub fn operation_kind_name(kind: i32) -> Option<&'static str> {
    let name = match v1::OperationKind::try_from(kind) {
        Ok(v1::OperationKind::Unspecified) => return None,
        Ok(v1::OperationKind::Snapshot) => "snapshot",
        Ok(v1::OperationKind::CreateWorkspace) => "create-workspace",
        Ok(v1::OperationKind::ReleaseWorkspace) => "release-workspace",
        Ok(v1::OperationKind::MoveWorkspace) => "move-workspace",
        Ok(v1::OperationKind::Describe) => "describe",
        Ok(v1::OperationKind::Abandon) => "abandon",
        Ok(v1::OperationKind::Undo) => "undo",
        Ok(v1::OperationKind::Edit) => "edit",
        Ok(v1::OperationKind::ImportRefs) => "import-refs",
        Ok(v1::OperationKind::External) => "external",
        Ok(v1::OperationKind::Other) => "other",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// [`snapshot_kind_name`], `None` when unset: an operation that is not a
/// snapshot has no snapshot kind.
pub fn snapshot_kind_opt(kind: i32) -> Option<&'static str> {
    (kind != i32::from(v1::SnapshotKind::Unspecified)).then(|| snapshot_kind_name(kind))
}

fn name(value: Option<&'static str>) -> Option<String> {
    value.map(str::to_owned)
}

/// An author or committer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Signature {
    /// Their name.
    pub name: String,
    /// Their email.
    pub email: String,
    /// When.
    pub at: Option<String>,
}

impl From<&v1::Signature> for Signature {
    fn from(signature: &v1::Signature) -> Self {
        Self {
            name: signature.name.clone(),
            email: signature.email.clone(),
            at: time(signature.at.as_ref()),
        }
    }
}

/// One commit of a change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[expect(clippy::struct_excessive_bools, reason = "the v1 message's flags")]
pub struct Change {
    /// The change, in hex.
    pub change_id: String,
    /// The same, in jj's reverse hex.
    pub change_id_reverse_hex: String,
    /// The commit.
    pub commit_id: String,
    /// Its parents' commits.
    pub parent_commit_ids: Vec<String>,
    /// Its parents' changes, in hex.
    pub parent_change_ids: Vec<String>,
    /// Its description, verbatim.
    pub description: String,
    /// Its author.
    pub author: Option<Signature>,
    /// Its committer.
    pub committer: Option<Signature>,
    /// Its tree; `null` when the tree is conflicted.
    pub tree_id: Option<String>,
    /// Its tree is its parents'.
    pub empty: bool,
    /// Its tree has conflicts.
    pub conflict: bool,
    /// Its change has more than one visible commit.
    pub divergent: bool,
    /// It is in `immutable()`.
    pub immutable: bool,
    /// It is not visible at head.
    pub hidden: bool,
    /// Local bookmarks on it.
    pub bookmarks: Vec<String>,
    /// Tags on it.
    pub tags: Vec<String>,
    /// Reef workspaces whose working copy it is.
    pub workspace_ids: Vec<String>,
    /// jj workspaces whose working copy it is.
    pub jj_workspaces: Vec<String>,
}

impl From<&v1::Change> for Change {
    fn from(change: &v1::Change) -> Self {
        Self {
            change_id: change.change_id.clone(),
            change_id_reverse_hex: reverse_hex(&change.change_id),
            commit_id: change.commit_id.clone(),
            parent_commit_ids: change.parent_commit_ids.clone(),
            parent_change_ids: change.parent_change_ids.clone(),
            description: change.description.clone(),
            author: change.author.as_ref().map(Signature::from),
            committer: change.committer.as_ref().map(Signature::from),
            tree_id: text(&change.tree_id),
            empty: change.empty,
            conflict: change.conflict,
            divergent: change.divergent,
            immutable: change.immutable,
            hidden: change.hidden,
            bookmarks: change.bookmarks.clone(),
            tags: change.tags.clone(),
            workspace_ids: change.workspace_ids.clone(),
            jj_workspaces: change.jj_workspaces.clone(),
        }
    }
}

fn change_or_default(change: Option<&v1::Change>) -> Change {
    change.map_or_else(|| Change::from(&v1::Change::default()), Change::from)
}

/// An edge of the log's graph, from an entry to an ancestor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct GraphEdge {
    /// The ancestor's commit.
    pub commit_id: String,
    /// `direct`, `indirect`, or `missing`.
    pub kind: Option<String>,
}

/// One entry of `reef log`: a change and its graph edges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct LogEntry {
    /// The change.
    #[serde(flatten)]
    pub change: Change,
    /// Its edges to the ancestors in the log.
    pub edges: Vec<GraphEdge>,
}

impl From<&v1::LogEntry> for LogEntry {
    fn from(entry: &v1::LogEntry) -> Self {
        Self {
            change: change_or_default(entry.change.as_ref()),
            edges: entry
                .edges
                .iter()
                .map(|edge| GraphEdge {
                    commit_id: edge.commit_id.clone(),
                    kind: name(graph_edge_kind_name(edge.kind)),
                })
                .collect(),
        }
    }
}

/// An operation of the repository's log.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Operation {
    /// Its id.
    pub id: String,
    /// Its repository.
    pub repo_id: String,
    /// Its parents.
    pub parent_ids: Vec<String>,
    /// Its description.
    pub description: String,
    /// When it started.
    pub started_at: Option<String>,
    /// When it ended.
    pub ended_at: Option<String>,
    /// The host it ran on.
    pub hostname: String,
    /// The user it ran as.
    pub username: String,
    /// Whether it is a working-copy snapshot.
    pub is_snapshot: bool,
    /// Its `reef.kind`.
    pub kind: Option<String>,
    /// For a snapshot: `watcher`, `explicit`, `final`, `move`, or
    /// `shutdown`.
    pub snapshot_kind: Option<String>,
    /// The Reef workspace it ran for.
    pub workspace_id: Option<String>,
    /// The jj workspace it ran for.
    pub jj_workspace: Option<String>,
    /// The agent it ran for.
    pub agent: Option<String>,
    /// jj's operation attributes, verbatim.
    pub attributes: BTreeMap<String, String>,
    /// What it changed; `null` unless asked for.
    pub effects: Option<OperationEffects>,
}

impl From<&v1::Operation> for Operation {
    fn from(op: &v1::Operation) -> Self {
        Self {
            id: op.id.clone(),
            repo_id: op.repo_id.clone(),
            parent_ids: op.parent_ids.clone(),
            description: op.description.clone(),
            started_at: time(op.started_at.as_ref()),
            ended_at: time(op.ended_at.as_ref()),
            hostname: op.hostname.clone(),
            username: op.username.clone(),
            is_snapshot: op.is_snapshot,
            kind: name(operation_kind_name(op.kind)),
            snapshot_kind: name(snapshot_kind_opt(op.snapshot_kind)),
            workspace_id: text(&op.workspace_id),
            jj_workspace: text(&op.jj_workspace),
            agent: text(&op.agent),
            attributes: op.attributes.clone(),
            effects: op.effects.as_ref().map(OperationEffects::from),
        }
    }
}

/// What an operation changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct OperationEffects {
    /// Changes it created, rewrote, abandoned, or hid.
    pub changes: Vec<ChangeEffect>,
    /// Bookmarks it moved.
    pub bookmarks: Vec<BookmarkEffect>,
    /// Working copies it moved.
    pub working_copies: Vec<WorkingCopyEffect>,
    /// How many changes, when `changes` is cut.
    pub change_count: u32,
    /// How many bookmarks, when `bookmarks` is cut.
    pub bookmark_count: u32,
}

/// What an operation did to one change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChangeEffect {
    /// The change, in hex.
    pub change_id: String,
    /// The same, in reverse hex.
    pub change_id_reverse_hex: String,
    /// `created`, `rewritten`, `abandoned`, or `hidden`.
    pub kind: Option<String>,
    /// Its commit afterwards.
    pub commit_id: Option<String>,
    /// Its commit before.
    pub previous_commit_id: Option<String>,
}

/// What an operation did to one bookmark.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BookmarkEffect {
    /// The bookmark.
    pub name: String,
    /// Its remote; `null` for a local bookmark.
    pub remote: Option<String>,
    /// Its commit afterwards.
    pub commit_id: Option<String>,
    /// Its commit before.
    pub previous_commit_id: Option<String>,
    /// Whether it is conflicted afterwards.
    pub conflicted: bool,
}

/// What an operation did to one working copy.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkingCopyEffect {
    /// The Reef workspace.
    pub workspace_id: Option<String>,
    /// The jj workspace.
    pub jj_workspace: String,
    /// Its commit afterwards.
    pub commit_id: Option<String>,
    /// Its commit before.
    pub previous_commit_id: Option<String>,
}

impl From<&v1::OperationEffects> for OperationEffects {
    fn from(effects: &v1::OperationEffects) -> Self {
        Self {
            changes: effects
                .changes
                .iter()
                .map(|change| ChangeEffect {
                    change_id: change.change_id.clone(),
                    change_id_reverse_hex: reverse_hex(&change.change_id),
                    kind: name(change_effect_kind_name(change.kind)),
                    commit_id: text(&change.commit_id),
                    previous_commit_id: text(&change.previous_commit_id),
                })
                .collect(),
            bookmarks: effects
                .bookmarks
                .iter()
                .map(|bookmark| BookmarkEffect {
                    name: bookmark.name.clone(),
                    remote: text(&bookmark.remote),
                    commit_id: text(&bookmark.commit_id),
                    previous_commit_id: text(&bookmark.previous_commit_id),
                    conflicted: bookmark.conflicted,
                })
                .collect(),
            working_copies: effects
                .working_copies
                .iter()
                .map(|wc| WorkingCopyEffect {
                    workspace_id: text(&wc.workspace_id),
                    jj_workspace: wc.jj_workspace.clone(),
                    commit_id: text(&wc.commit_id),
                    previous_commit_id: text(&wc.previous_commit_id),
                })
                .collect(),
            change_count: effects.change_count,
            bookmark_count: effects.bookmark_count,
        }
    }
}

fn operation(op: Option<&v1::Operation>) -> Option<Operation> {
    op.map(Operation::from)
}

/// A path a diff or a status reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileChange {
    /// Repository-relative, `/`-separated.
    pub path: String,
    /// `added`, `modified`, `deleted`, `renamed`, or `copied`.
    pub kind: Option<String>,
    /// Where a rename or copy came from.
    pub old_path: Option<String>,
    /// The type before; `null` when added.
    pub old_type: Option<String>,
    /// The type after; `null` when deleted.
    pub new_type: Option<String>,
}

impl From<&v1::FileChange> for FileChange {
    fn from(change: &v1::FileChange) -> Self {
        Self {
            path: change.path.clone(),
            kind: name(file_change_kind_name(change.kind)),
            old_path: text(&change.old_path),
            old_type: name(entry_type_name(change.old_type)),
            new_type: name(entry_type_name(change.new_type)),
        }
    }
}

/// A hunk of a file's diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Hunk {
    /// Its first line on the old side, from 1.
    pub old_start: u32,
    /// Its lines on the old side.
    pub old_lines: u32,
    /// Its first line on the new side, from 1.
    pub new_start: u32,
    /// Its lines on the new side.
    pub new_lines: u32,
    /// Its lines, each prefixed ` `, `-`, or `+`.
    pub lines: String,
}

/// One file of a diff.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[expect(clippy::struct_excessive_bools, reason = "the v1 message's flags")]
pub struct FileDiff {
    /// The path and how it changed.
    #[serde(flatten)]
    pub change: FileChange,
    /// Either side is binary.
    pub binary: bool,
    /// Either side is conflicted.
    pub conflict: bool,
    /// Lines added; 0 with `--summary`.
    pub lines_added: u32,
    /// Lines removed; 0 with `--summary`.
    pub lines_removed: u32,
    /// Its hunks; empty with `--summary` and `--stat`.
    pub hunks: Vec<Hunk>,
    /// The hunks were cut at the line limit.
    pub truncated: bool,
    /// The old side has no final newline.
    pub old_no_final_newline: bool,
    /// The new side has no final newline.
    pub new_no_final_newline: bool,
    /// A side is over the size the daemon diffs.
    pub too_large: bool,
    /// A side is not UTF-8; its lines are lossy.
    pub invalid_utf8: bool,
}

impl From<&v1::FileDiff> for FileDiff {
    fn from(diff: &v1::FileDiff) -> Self {
        Self {
            change: FileChange::from(&diff.change.clone().unwrap_or_default()),
            binary: diff.binary,
            conflict: diff.conflict,
            lines_added: diff.lines_added,
            lines_removed: diff.lines_removed,
            hunks: diff
                .hunks
                .iter()
                .map(|hunk| Hunk {
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
}

/// `reef diff`: every file of a diff between two trees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Diff {
    /// The old side: one commit, or several merged.
    pub from_commit_ids: Vec<String>,
    /// The new side.
    pub to_commit_id: String,
    /// The operation read at.
    pub at_operation: String,
    /// Files that differ.
    pub total_files: u32,
    /// Each of them.
    pub files: Vec<FileDiff>,
}

impl From<&v1::GetDiffResponse> for Diff {
    fn from(diff: &v1::GetDiffResponse) -> Self {
        Self {
            from_commit_ids: diff.from_commit_ids.clone(),
            to_commit_id: diff.to_commit_id.clone(),
            at_operation: diff.at_operation.clone(),
            total_files: diff.total_files,
            files: diff.files.iter().map(FileDiff::from).collect(),
        }
    }
}

/// `reef show`: a change and the paths it changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ChangeSummary {
    /// The change.
    #[serde(flatten)]
    pub change: Change,
    /// The operation read at.
    pub at_operation: String,
    /// The paths it changes against its parents.
    pub files: Vec<FileChange>,
    /// How many.
    pub total_files: u32,
}

impl ChangeSummary {
    /// From `GetChange`'s answer and the summary diff of its commit.
    pub fn from_responses(change: &v1::GetChangeResponse, diff: &v1::GetDiffResponse) -> Self {
        Self {
            change: change_or_default(change.change.as_ref()),
            at_operation: change.at_operation.clone(),
            files: diff
                .files
                .iter()
                .map(|file| FileChange::from(&file.change.clone().unwrap_or_default()))
                .collect(),
            total_files: diff.total_files,
        }
    }
}

/// `reef status`: the repository, and the workspace when there is one.
/// (`Status` is `daemon status`'s.)
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepoStatus {
    /// The repository.
    pub repository: Option<Repository>,
    /// The operation read at.
    pub at_operation: String,
    /// The repository's state.
    pub repository_status: Option<RepositoryStatus>,
    /// The workspace's; `null` in repository scope.
    pub workspace_status: Option<WorkspaceStatus>,
}

/// A repository's state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RepositoryStatus {
    /// `trunk()`.
    pub trunk: Option<Change>,
    /// The bookmark `trunk()` resolved through.
    pub trunk_bookmark: Option<String>,
    /// Its active workspaces, newest first, up to the cap.
    pub active_workspaces: Vec<Workspace>,
    /// How many.
    pub active_workspace_count: u32,
    /// The visible heads, up to the cap.
    pub heads: Vec<Change>,
    /// How many.
    pub head_count: u32,
    /// Mutable changes with conflicts, in hex.
    pub conflicted_change_ids: Vec<String>,
    /// Divergent changes, in hex.
    pub divergent_change_ids: Vec<String>,
    /// The head operation.
    pub last_operation: Option<Operation>,
}

/// A workspace's state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct WorkspaceStatus {
    /// The workspace.
    pub workspace: Option<Workspace>,
    /// Its working-copy commit.
    pub working_copy: Option<Change>,
    /// That commit's parents.
    pub parents: Vec<Change>,
    /// Paths it changes against them, up to `--max-files`.
    pub changed_files: Vec<FileChange>,
    /// How many.
    pub changed_file_count: u32,
    /// Conflicted paths, up to the cap.
    pub conflicted_paths: Vec<String>,
    /// How many.
    pub conflicted_path_count: u32,
    /// `snapshot`, `index`, `walk`, or `last-snapshot`.
    pub source: Option<String>,
    /// The snapshot the status took, if it recorded one.
    pub snapshot: Option<Operation>,
}

impl From<&v1::GetStatusResponse> for RepoStatus {
    fn from(status: &v1::GetStatusResponse) -> Self {
        Self {
            repository: status.repository.as_ref().map(Repository::from),
            at_operation: status.at_operation.clone(),
            repository_status: status
                .repository_status
                .as_ref()
                .map(|repo| RepositoryStatus {
                    trunk: repo.trunk.as_ref().map(Change::from),
                    trunk_bookmark: text(&repo.trunk_bookmark),
                    active_workspaces: repo.active_workspaces.iter().map(Workspace::from).collect(),
                    active_workspace_count: repo.active_workspace_count,
                    heads: repo.heads.iter().map(Change::from).collect(),
                    head_count: repo.head_count,
                    conflicted_change_ids: repo.conflicted_change_ids.clone(),
                    divergent_change_ids: repo.divergent_change_ids.clone(),
                    last_operation: operation(repo.last_operation.as_ref()),
                }),
            workspace_status: status.workspace_status.as_ref().map(|ws| WorkspaceStatus {
                workspace: ws.workspace.as_ref().map(Workspace::from),
                working_copy: ws.working_copy.as_ref().map(Change::from),
                parents: ws.parents.iter().map(Change::from).collect(),
                changed_files: ws.changed_files.iter().map(FileChange::from).collect(),
                changed_file_count: ws.changed_file_count,
                conflicted_paths: ws.conflicted_paths.clone(),
                conflicted_path_count: ws.conflicted_path_count,
                source: name(status_source_name(ws.source)),
                snapshot: operation(ws.snapshot.as_ref()),
            }),
        }
    }
}

/// `reef describe`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Described {
    /// The change afterwards.
    pub change: Option<Change>,
    /// The operation; `null` when the description was already that.
    pub operation: Option<Operation>,
    /// The scope's workspace afterwards; `null` in repository scope.
    pub workspace: Option<Workspace>,
}

impl Described {
    /// From the response and the workspace read after it.
    pub fn from_response(
        response: &v1::DescribeChangeResponse,
        workspace: Option<Workspace>,
    ) -> Self {
        Self {
            change: response.change.as_ref().map(Change::from),
            operation: operation(response.operation.as_ref()),
            workspace,
        }
    }
}

/// `reef abandon`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Abandoned {
    /// The commit abandoned.
    pub abandoned_commit_id: String,
    /// The operation.
    pub operation: Option<Operation>,
    /// The scope's workspace afterwards; `null` in repository scope.
    pub workspace: Option<Workspace>,
}

impl Abandoned {
    /// From the response and the workspace read after it.
    pub fn from_response(
        response: &v1::AbandonChangeResponse,
        workspace: Option<Workspace>,
    ) -> Self {
        Self {
            abandoned_commit_id: response.abandoned_commit_id.clone(),
            operation: operation(response.operation.as_ref()),
            workspace,
        }
    }
}

/// `reef undo`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Undone {
    /// The operation that reverted it; `null` when there was nothing to
    /// revert.
    pub operation: Option<Operation>,
    /// The snapshot taken first, if it recorded one.
    pub snapshot: Option<Operation>,
    /// The operation undone, in full.
    pub undone_operation_id: String,
    /// The scope's workspace afterwards; `null` in repository scope.
    pub workspace: Option<Workspace>,
}

impl Undone {
    /// From the response and the workspace read after it.
    pub fn from_response(
        response: &v1::UndoOperationResponse,
        workspace: Option<Workspace>,
    ) -> Self {
        Self {
            operation: operation(response.operation.as_ref()),
            snapshot: operation(response.snapshot.as_ref()),
            undone_operation_id: response.undone_operation_id.clone(),
            workspace,
        }
    }
}

/// `reef ws snapshot`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Snapshotted {
    /// Whether anything changed.
    pub changed: bool,
    /// The working-copy commit afterwards.
    pub commit_id: String,
    /// How long the scan took.
    pub scan_ms: u64,
    /// The snapshot's operation; `null` when nothing changed.
    pub operation: Option<Operation>,
    /// The workspace afterwards.
    pub workspace: Option<Workspace>,
}

impl Snapshotted {
    /// From the response and the workspace read after it.
    pub fn from_response(
        response: &v1::SnapshotWorkspaceResponse,
        workspace: Option<Workspace>,
    ) -> Self {
        Self {
            changed: response.changed,
            commit_id: response.commit_id.clone(),
            scan_ms: response.scan_ms,
            operation: operation(response.operation.as_ref()),
            workspace,
        }
    }
}

/// `FileService.EditFiles`' answer, as `reef mcp`'s `edit_files` returns it
/// (R2.13).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Edited {
    /// The change afterwards.
    pub change: Option<Change>,
    /// The operation that recorded the edits.
    pub operation: Option<Operation>,
    /// The scope's workspace afterwards; `null` in repository scope.
    pub workspace: Option<Workspace>,
}

impl Edited {
    /// From the response and the workspace read after it.
    pub fn from_response(response: &v1::EditFilesResponse, workspace: Option<Workspace>) -> Self {
        Self {
            change: response.change.as_ref().map(Change::from),
            operation: operation(response.operation.as_ref()),
            workspace,
        }
    }
}

/// An entry of `reef file list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileEntry {
    /// Repository-relative, `/`-separated.
    pub path: String,
    /// `file`, `executable`, `symlink`, `directory`, `conflict`, or
    /// `submodule`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Its content's id; `null` for a directory or conflict.
    pub file_id: Option<String>,
}

impl From<&v1::FileEntry> for FileEntry {
    fn from(entry: &v1::FileEntry) -> Self {
        Self {
            path: entry.path.clone(),
            kind: name(entry_type_name(entry.r#type)),
            file_id: text(&entry.file_id),
        }
    }
}

/// `reef file read -o json`: the file and its bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct FileRead {
    /// Repository-relative, `/`-separated.
    pub path: String,
    /// `file`, `executable`, `symlink`, or `conflict`.
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Its content's id.
    pub file_id: Option<String>,
    /// Its size in bytes.
    pub size: u64,
    /// The commit read.
    pub commit_id: String,
    /// The operation read at.
    pub at_operation: String,
    /// The bytes, when they are UTF-8; else `null`.
    pub content: Option<String>,
    /// The bytes in standard padded base64, when they are not UTF-8; else
    /// `null`.
    pub content_base64: Option<String>,
}

impl From<&v1::ReadFileResponse> for FileRead {
    fn from(read: &v1::ReadFileResponse) -> Self {
        let entry = read.entry.clone().unwrap_or_default();
        let (content, content_base64) = match String::from_utf8(read.content.clone()) {
            Ok(text) => (Some(text), None),
            Err(error) => (None, Some(STANDARD.encode(error.into_bytes()))),
        };
        Self {
            path: entry.path,
            kind: name(entry_type_name(entry.r#type)),
            file_id: text(&entry.file_id),
            size: read.size,
            commit_id: read.commit_id.clone(),
            at_operation: read.at_operation.clone(),
            content,
            content_base64,
        }
    }
}

/// `{"error": {...}}`: a failed command's JSON on stdout (ADR 0025), and
/// what a `reef-daemon` offline command prints when it fails.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorObject {
    /// The kind's `snake_case` name ([`crate::error::kind_text`]).
    pub kind: String,
    /// The daemon's `ErrorDetail.reason` (R2.01), when it has one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub reason: String,
    /// The error and its sources, as one line.
    pub message: String,
    /// How to fix it, one sentence.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hint: String,
    /// The command that fixes it, an argv starting `reef`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next: Vec<String>,
    /// The values that go with `reason`, `ErrorDetail.metadata` (R2.27).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

/// [`ErrorObject`] under its `error` key.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorDocument {
    /// The error.
    pub error: ErrorObject,
}

/// What `reef restore` prints (R1.18).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Restored {
    /// The repositories restored.
    pub repos: Vec<RestoredRepo>,
    /// Active workspaces kept, rematerialized when the daemon starts.
    pub workspaces_kept: u64,
    /// Leases extended so their owners can come back.
    pub leases_extended: u64,
    /// Whether a repository's default workspace was recreated.
    pub default_workspace_recreated: bool,
    /// What was replaced, kept under `.replaced-<nanos>` names.
    pub replaced: Vec<String>,
}

/// One repository `reef restore` restored.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestoredRepo {
    /// Its id, in hex.
    pub id: String,
    /// Its `.jj/repo`.
    pub jj_repo_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_hex_matches_jj() {
        assert_eq!(
            reverse_hex("0123456789abcdeffedcba9876543210"),
            "zyxwvutsrqponmlkklmnopqrstuvwxyz"
        );
    }

    #[test]
    fn workspace_json_has_stable_names_and_nulls() {
        let ws = v1::Workspace {
            id: "00000000000000aa".into(),
            change_id: "01".into(),
            state: v1::WorkspaceState::Active.into(),
            created_at: Some(prost_types::Timestamp {
                seconds: 1_792_175_477,
                nanos: 42_000_000,
            }),
            lease: Some(v1::Lease {
                id: "00000000000000bb".into(),
                expires_at: Some(prost_types::Timestamp {
                    seconds: 1_792_175_478,
                    nanos: 0,
                }),
                state: v1::LeaseState::Active.into(),
                ..v1::Lease::default()
            }),
            ..v1::Workspace::default()
        };
        let value = serde_json::to_value(Workspace::from(&ws)).unwrap();
        assert_eq!(value["state"], "active");
        assert_eq!(value["backend"], UNKNOWN);
        assert_eq!(value["watch_state"], serde_json::Value::Null);
        assert_eq!(value["id"], "00000000000000aa");
        assert_eq!(value["change_id_reverse_hex"], "zy");
        assert_eq!(value["created_at"], "2026-10-16T18:31:17.042Z");
        assert_eq!(value["attention"], serde_json::Value::Null);
        assert_eq!(value["last_snapshot_at"], serde_json::Value::Null);
        assert_eq!(value["lease"]["expires_at"], "2026-10-16T18:31:18Z");
        assert!(value["lease"].get("expires_at_ms").is_none());
        assert_eq!(value["lease"]["state"], "active");
        assert_eq!(value["lease"]["end_reason"], serde_json::Value::Null);
        assert_eq!(
            value["lease"]["heartbeat_every_ms"],
            serde_json::Value::Null
        );
        assert_eq!(value["stores"], serde_json::json!([]));
    }

    #[test]
    fn enums_print_as_phase_1_strings() {
        assert_eq!(
            backend_name(v1::Backend::FuseOverlayfs.into()),
            "fuse-overlayfs"
        );
        assert_eq!(backend_name(v1::Backend::Apfs.into()), "apfs");
        assert_eq!(backend_name(99), UNKNOWN);
        assert_eq!(
            attention_name(v1::Attention::StaleRecovered.into()),
            Some("stale-recovered")
        );
        assert_eq!(attention_name(v1::Attention::Unspecified.into()), None);
        assert_eq!(attention_name(99), Some(UNKNOWN));
        assert_eq!(snapshot_kind_name(v1::SnapshotKind::Final.into()), "final");
        assert_eq!(
            watch_state_name(v1::WatchState::Attaching.into()),
            Some("attaching")
        );
        assert_eq!(watch_state_name(v1::WatchState::Attached.into()), None);
        assert_eq!(
            lease_end_reason(v1::LeaseEndReason::Restored.into()),
            Some("restored")
        );
    }

    #[test]
    fn mounted_stores_name_their_mode() {
        let ws = v1::Workspace {
            stores: vec![v1::MountedStore {
                name: "deps".into(),
                lockfile_hash: "ab".into(),
                mount: "vendor/deps".into(),
                mode: v1::StoreMountMode::Bind.into(),
                verified: true,
            }],
            ..v1::Workspace::default()
        };
        let value = serde_json::to_value(Workspace::from(&ws)).unwrap();
        assert_eq!(
            value["stores"],
            serde_json::json!([{
                "name": "deps",
                "lockfile_hash": "ab",
                "mount": "vendor/deps",
                "mode": "bind",
                "verified": true,
            }])
        );
    }

    /// R2.27 test 22: every value of every new enum by name, an unknown
    /// value as `unknown`, an unset one as `None`.
    #[test]
    fn vcs_enums_print_their_names() {
        type Name = fn(i32) -> Option<&'static str>;
        let tables: [(Name, &[&str]); 7] = [
            (
                entry_type_name,
                &[
                    "file",
                    "executable",
                    "symlink",
                    "directory",
                    "conflict",
                    "submodule",
                ],
            ),
            (
                file_change_kind_name,
                &["added", "modified", "deleted", "renamed", "copied"],
            ),
            (
                status_source_name,
                &["snapshot", "index", "walk", "last-snapshot"],
            ),
            (graph_edge_kind_name, &["direct", "indirect", "missing"]),
            (
                change_effect_kind_name,
                &["created", "rewritten", "abandoned", "hidden"],
            ),
            (
                operation_kind_name,
                &[
                    "snapshot",
                    "create-workspace",
                    "release-workspace",
                    "move-workspace",
                    "describe",
                    "abandon",
                    "undo",
                    "edit",
                    "import-refs",
                    "external",
                    "other",
                ],
            ),
            (
                snapshot_kind_opt,
                &["watcher", "explicit", "final", "move", "shutdown"],
            ),
        ];
        for (name, values) in tables {
            assert_eq!(name(0), None);
            assert_eq!(name(99), Some(UNKNOWN));
            for (value, expected) in (1..).zip(values.iter()) {
                assert_eq!(name(value), Some(*expected));
            }
        }
    }

    #[test]
    fn file_reads_carry_text_or_base64() {
        let read = |content: &[u8]| {
            serde_json::to_value(FileRead::from(&v1::ReadFileResponse {
                entry: Some(v1::FileEntry {
                    path: "a".into(),
                    r#type: v1::EntryType::File.into(),
                    file_id: "ff".into(),
                }),
                content: content.to_vec(),
                size: 3,
                commit_id: "c0".into(),
                at_operation: "0p".into(),
            }))
            .unwrap()
        };
        let text = read(b"hi\n");
        assert_eq!(text["content"], "hi\n");
        assert_eq!(text["content_base64"], serde_json::Value::Null);
        assert_eq!(text["type"], "file");
        let bytes = read(&[0xff, 0x00, 0x80]);
        assert_eq!(bytes["content"], serde_json::Value::Null);
        assert_eq!(bytes["content_base64"], "/wCA");
    }

    #[test]
    fn operations_print_effects_only_when_computed() {
        let op = v1::Operation {
            id: "0p".into(),
            kind: v1::OperationKind::Describe.into(),
            ..v1::Operation::default()
        };
        let plain = serde_json::to_value(Operation::from(&op)).unwrap();
        assert_eq!(plain["effects"], serde_json::Value::Null);
        assert_eq!(plain["kind"], "describe");
        assert_eq!(plain["snapshot_kind"], serde_json::Value::Null);
        assert_eq!(plain["workspace_id"], serde_json::Value::Null);
        let with = v1::Operation {
            effects: Some(v1::OperationEffects {
                changes: vec![v1::ChangeEffect {
                    change_id: "01".into(),
                    kind: v1::ChangeEffectKind::Rewritten.into(),
                    commit_id: "c1".into(),
                    previous_commit_id: "c0".into(),
                }],
                change_count: 1,
                ..v1::OperationEffects::default()
            }),
            ..op
        };
        let value = serde_json::to_value(Operation::from(&with)).unwrap();
        let change = &value["effects"]["changes"][0];
        assert_eq!(change["change_id_reverse_hex"], "zy");
        assert_eq!(change["kind"], "rewritten");
        assert_eq!(change["previous_commit_id"], "c0");
    }

    #[test]
    fn error_objects_print_metadata_only_when_there_is_some() {
        let mut error = ErrorObject {
            kind: "conflict".into(),
            message: "m".into(),
            ..ErrorObject::default()
        };
        let value = serde_json::to_value(&error).unwrap();
        assert!(value.get("metadata").is_none(), "{value}");
        error.metadata.insert("commit_id".into(), "c0".into());
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["metadata"]["commit_id"], "c0");
        // The helper protocol still parses a document without it.
        let parsed: ErrorObject =
            serde_json::from_str(r#"{"kind": "io", "message": "m"}"#).unwrap();
        assert!(parsed.metadata.is_empty());
    }

    #[test]
    fn created_flattens_the_workspace() {
        let response = v1::CreateWorkspaceResponse {
            workspace: Some(v1::Workspace {
                id: "00000000000000aa".into(),
                path: "/ws".into(),
                ..v1::Workspace::default()
            }),
            operation: Some(v1::Operation {
                id: "ff".into(),
                ..v1::Operation::default()
            }),
            timings: Some(v1::CreateTimings {
                base_ms: 2,
                materialize_ms: 3,
                ..v1::CreateTimings::default()
            }),
            ..v1::CreateWorkspaceResponse::default()
        };
        let value = serde_json::to_value(Created::from_response(&response).unwrap()).unwrap();
        assert_eq!(value["id"], "00000000000000aa");
        assert_eq!(value["path"], "/ws");
        assert_eq!(value["op_id"], "ff");
        assert_eq!(value["materialize_ms"], 5);
        assert!(Created::from_response(&v1::CreateWorkspaceResponse::default()).is_none());
    }
}

/// How far a long-running operation has got (R2.10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Progress {
    /// What it is doing now.
    pub phase: Option<String>,
    /// Units done.
    pub done: u64,
    /// Units in all; `None` when unknown.
    pub total: Option<u64>,
    /// What a unit is.
    pub unit: Option<String>,
}

/// One presence entry (R2.10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Presence {
    /// The entry's id.
    pub id: String,
    /// Who reported it.
    pub agent: String,
    /// The repository it is about.
    pub repo_id: Option<String>,
    /// The workspace it is about.
    pub workspace_id: Option<String>,
    /// The change it is about, in hex.
    pub change_id: Option<String>,
    /// `activity`, `editing`, `lease`, or `progress`.
    pub kind: String,
    /// For `activity`.
    pub activity: Option<String>,
    /// For `editing`: repository-relative paths, most recent first.
    pub paths: Vec<String>,
    /// For `progress`.
    pub progress: Option<Progress>,
    /// For `lease`: the lease as of the heartbeat.
    pub lease: Option<Lease>,
    /// Reported by the daemon itself.
    pub by_daemon: bool,
    /// When it was last reported.
    pub updated_at: Option<String>,
    /// When it lapses unless reported again.
    pub expires_at: Option<String>,
    /// Unix milliseconds of `updated_at`, to tell a newer state.
    #[serde(skip)]
    pub updated_at_ms: i64,
}

/// A presence kind's JSON name.
pub fn presence_kind(kind: i32) -> &'static str {
    match v1::PresenceKind::try_from(kind) {
        Ok(v1::PresenceKind::Activity) => "activity",
        Ok(v1::PresenceKind::Editing) => "editing",
        Ok(v1::PresenceKind::Lease) => "lease",
        Ok(v1::PresenceKind::Progress) => "progress",
        _ => UNKNOWN,
    }
}

impl From<&v1::Presence> for Presence {
    fn from(presence: &v1::Presence) -> Self {
        Self {
            id: presence.id.clone(),
            agent: presence.agent.clone(),
            repo_id: text(&presence.repo_id),
            workspace_id: text(&presence.workspace_id),
            change_id: text(&presence.change_id),
            kind: presence_kind(presence.kind).to_owned(),
            activity: text(&presence.activity),
            paths: presence.paths.clone(),
            progress: presence.progress.as_ref().map(|progress| Progress {
                phase: text(&progress.phase),
                done: progress.done,
                total: (progress.total > 0).then_some(progress.total),
                unit: text(&progress.unit),
            }),
            lease: presence.lease.as_ref().map(Lease::from),
            by_daemon: presence.by_daemon,
            updated_at: time(presence.updated_at.as_ref()),
            expires_at: time(presence.expires_at.as_ref()),
            updated_at_ms: presence.updated_at.as_ref().map_or(0, millis_of),
        }
    }
}

/// A change to one presence entry, as `reef presence watch` prints it
/// (R2.10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PresenceEvent {
    /// It ended: cleared, expired, or what it is about is gone.
    pub ended: bool,
    /// `cleared`, `expired`, or `gone`; `None` while it lives, or when the
    /// reason is not known (an entry that vanished across a restart).
    pub end_reason: Option<String>,
    /// The entry; its last state when ended.
    pub presence: Presence,
}

impl From<&v1::PresenceEvent> for PresenceEvent {
    fn from(event: &v1::PresenceEvent) -> Self {
        let end_reason = match v1::PresenceEndReason::try_from(event.end_reason) {
            Ok(v1::PresenceEndReason::Cleared) => Some("cleared"),
            Ok(v1::PresenceEndReason::Expired) => Some("expired"),
            Ok(v1::PresenceEndReason::Gone) => Some("gone"),
            _ => None,
        };
        Self {
            ended: event.ended,
            end_reason: end_reason.map(str::to_owned),
            presence: event
                .presence
                .as_ref()
                .map_or_else(|| Presence::from(&v1::Presence::default()), Presence::from),
        }
    }
}
