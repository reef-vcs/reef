//! The JSON form of the API's responses, as `reef -o json` prints them
//! (R1.22): serde structs built from the generated messages, so the field
//! names are fixed `snake_case` whatever the generated code calls them,
//! timestamps are RFC 3339 UTC strings, ids are full lowercase hex, and an
//! absent value (an empty string or unset field in the message) is `null`.
//!
//! These are output types only; nothing parses them back.

use serde::Serialize;

use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::proto::v1;

/// What `CreateResponse.start` says when no bookmark exists and the
/// default start fell back to the `default` workspace's working-copy commit
/// (R1.31).
pub const ROOT_CHECKOUT: &str = "root checkout";

/// A `google.protobuf.Timestamp` as RFC 3339 with millisecond precision,
/// trailing zeros of the fraction dropped (`2026-10-16T18:31:17.042Z`,
/// `2026-10-16T18:31:18Z`). A time `time` cannot represent is
/// `<millis>ms`.
pub fn rfc3339(time: &prost_types::Timestamp) -> String {
    rfc3339_millis(millis_of(time))
}

/// Unix milliseconds as [`rfc3339`] formats them.
pub fn rfc3339_millis(millis: i64) -> String {
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
fn lease_state(state: v1::lease::State) -> &'static str {
    match state {
        v1::lease::State::Active => "active",
        v1::lease::State::InGrace => "in_grace",
        v1::lease::State::Expired => "expired",
        v1::lease::State::Ended => "ended",
    }
}

/// A `google.protobuf.Duration` in whole milliseconds; negative is zero.
fn duration_millis(duration: &prost_types::Duration) -> u64 {
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
            length_ms: lease.length_ms,
            grace_ms: lease.grace_ms,
            state: lease_state(lease.state()).to_owned(),
            end_reason: text(&lease.end_reason),
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
        let mode = match store.mode() {
            v1::mounted_store::Mode::Symlink => "symlink",
            v1::mounted_store::Mode::Bind => "bind",
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
            backend: ws.backend.clone(),
            path: ws.path.clone(),
            state: ws.state.clone(),
            divergent: ws.divergent,
            attention: text(&ws.attention),
            created_at: time(ws.created_at.as_ref()),
            last_snapshot_at: time(ws.last_snapshot_at.as_ref()),
            last_snapshot_op: text(&ws.last_snapshot_op),
            lease: ws.lease.as_ref().map(Lease::from),
            watcher: ws.watcher.clone(),
            watch_state: text(&ws.watch_state),
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
            op_id: entry.op_id.clone(),
            commit_id: entry.commit_id.clone(),
            kind: entry.kind.clone(),
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

/// `Workspaces.Create`'s answer: the workspace's fields, then the create's.
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
    pub fn from_response(response: &v1::CreateResponse) -> Option<Self> {
        Some(Self {
            workspace: Workspace::from(response.workspace.as_ref()?),
            op_id: text(&response.op_id),
            shared_change: response.shared_change,
            materialize_ms: response.materialize_ms,
            base_was_cached: response.base_was_cached,
            timings: response
                .timings
                .as_ref()
                .map(CreateTimings::from)
                .unwrap_or_default(),
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

/// `Workspaces.Move`'s answer (R1.06): the workspace's fields, then the
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
    pub fn from_response(response: &v1::MoveResponse) -> Option<Self> {
        Some(Self {
            workspace: Workspace::from(response.workspace.as_ref()?),
            op_id: text(&response.op_id),
            final_op_id: text(&response.final_op_id),
            files_updated: response.files_updated,
            files_added: response.files_added,
            files_removed: response.files_removed,
            tree_replaced: response.tree_replaced,
        })
    }
}

/// `Workspaces.Inspect`'s answer: the workspace's fields, then the rest.
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
    pub fn from_response(response: &v1::InspectResponse) -> Option<Self> {
        Some(Self {
            workspace: Workspace::from(response.workspace.as_ref()?),
            disk_usage: response.disk_usage.as_ref().map(DiskUsage::from),
            other_workspaces_on_change: response.other_workspaces_on_change.clone(),
            op_id: text(&response.op_id),
            snapshot_count: response.snapshot_count,
            recent_snapshots: response
                .recent_snapshots
                .iter()
                .map(SnapshotEntry::from)
                .collect(),
        })
    }
}

/// `Workspaces.Release`'s answer.
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

impl From<&v1::ReleaseResponse> for Released {
    fn from(response: &v1::ReleaseResponse) -> Self {
        Self {
            id: response.workspace_id.clone(),
            final_op_id: text(&response.final_op_id),
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
            kind: probe.kind.clone(),
            available: probe.available,
            reason: text(&probe.reason),
            filesystem: probe.filesystem.clone(),
        }
    }
}

/// `Daemon.Status`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Status {
    /// The daemon's version.
    pub version: String,
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

impl From<&v1::StatusResponse> for Status {
    fn from(status: &v1::StatusResponse) -> Self {
        Self {
            version: status.version.clone(),
            pid: status.pid,
            uptime_ms: status.uptime_ms,
            socket: status.socket.clone(),
            state_dir: status.state_dir.clone(),
            schema_version: status.schema_version,
            repositories: status.repositories,
            active_workspaces: status.active_workspaces,
            backends: status.backends.iter().map(BackendProbe::from).collect(),
        }
    }
}

/// A built store, from `Stores.List` (R1.20).
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

/// How a `Stores.Build` ended (R1.20).
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
            op_id: text(&result.op_id),
            dropped_lines: result.dropped_lines,
            already_built: result.already_built,
        }
    }
}

/// One repository's report from `Maintenance.Gc` (R1.17).
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
            state: "active".into(),
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
                ..v1::Lease::default()
            }),
            ..v1::Workspace::default()
        };
        let value = serde_json::to_value(Workspace::from(&ws)).unwrap();
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
    fn mounted_stores_name_their_mode() {
        let ws = v1::Workspace {
            stores: vec![v1::MountedStore {
                name: "deps".into(),
                lockfile_hash: "ab".into(),
                mount: "vendor/deps".into(),
                mode: v1::mounted_store::Mode::Bind.into(),
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

    #[test]
    fn created_flattens_the_workspace() {
        let response = v1::CreateResponse {
            workspace: Some(v1::Workspace {
                id: "00000000000000aa".into(),
                path: "/ws".into(),
                ..v1::Workspace::default()
            }),
            op_id: "ff".into(),
            ..v1::CreateResponse::default()
        };
        let value = serde_json::to_value(Created::from_response(&response).unwrap()).unwrap();
        assert_eq!(value["id"], "00000000000000aa");
        assert_eq!(value["path"], "/ws");
        assert_eq!(value["op_id"], "ff");
        assert!(Created::from_response(&v1::CreateResponse::default()).is_none());
    }
}
