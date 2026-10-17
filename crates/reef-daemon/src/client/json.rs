//! The JSON form of the API's responses, as `reef -o json` prints them
//! (R1.22): serde structs built from the generated messages, so the field
//! names are fixed `snake_case` whatever the generated code calls them,
//! timestamps are RFC 3339 UTC strings, ids are full lowercase hex, and an
//! absent value (an empty string or unset field in the message) is `null`.
//!
//! These are output types only; nothing parses them back.

use serde::Serialize;

use crate::model::Timestamp;
use crate::proto::v0;

/// A `google.protobuf.Timestamp` as RFC 3339 with millisecond precision.
pub fn rfc3339(time: &prost_types::Timestamp) -> String {
    Timestamp(millis_of(time)).to_rfc3339()
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

impl From<&v0::Repository> for Repository {
    fn from(repo: &v0::Repository) -> Self {
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
fn lease_state(state: v0::lease::State) -> &'static str {
    match state {
        v0::lease::State::Active => "active",
        v0::lease::State::InGrace => "in_grace",
        v0::lease::State::Expired => "expired",
        v0::lease::State::Ended => "ended",
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

impl From<&v0::Lease> for Lease {
    fn from(lease: &v0::Lease) -> Self {
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
}

impl From<&v0::Workspace> for Workspace {
    fn from(ws: &v0::Workspace) -> Self {
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

impl From<&v0::DiskUsage> for DiskUsage {
    fn from(usage: &v0::DiskUsage) -> Self {
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
}

impl Created {
    /// From the response; `None` if it carries no workspace.
    pub fn from_response(response: &v0::CreateResponse) -> Option<Self> {
        Some(Self {
            workspace: Workspace::from(response.workspace.as_ref()?),
            op_id: text(&response.op_id),
            shared_change: response.shared_change,
            materialize_ms: response.materialize_ms,
            base_was_cached: response.base_was_cached,
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
}

impl Inspected {
    /// From the response; `None` if it carries no workspace.
    pub fn from_response(response: &v0::InspectResponse) -> Option<Self> {
        Some(Self {
            workspace: Workspace::from(response.workspace.as_ref()?),
            disk_usage: response.disk_usage.as_ref().map(DiskUsage::from),
            other_workspaces_on_change: response.other_workspaces_on_change.clone(),
            op_id: text(&response.op_id),
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

impl From<&v0::ReleaseResponse> for Released {
    fn from(response: &v0::ReleaseResponse) -> Self {
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

impl From<&v0::BackendProbe> for BackendProbe {
    fn from(probe: &v0::BackendProbe) -> Self {
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

impl From<&v0::StatusResponse> for Status {
    fn from(status: &v0::StatusResponse) -> Self {
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
        let ws = v0::Workspace {
            id: "00000000000000aa".into(),
            change_id: "01".into(),
            state: "active".into(),
            created_at: Some(prost_types::Timestamp {
                seconds: 1_792_175_477,
                nanos: 42_000_000,
            }),
            lease: Some(v0::Lease {
                id: "00000000000000bb".into(),
                expires_at: Some(prost_types::Timestamp {
                    seconds: 1_792_175_478,
                    nanos: 0,
                }),
                ..v0::Lease::default()
            }),
            ..v0::Workspace::default()
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
    }

    #[test]
    fn created_flattens_the_workspace() {
        let response = v0::CreateResponse {
            workspace: Some(v0::Workspace {
                id: "00000000000000aa".into(),
                path: "/ws".into(),
                ..v0::Workspace::default()
            }),
            op_id: "ff".into(),
            ..v0::CreateResponse::default()
        };
        let value = serde_json::to_value(Created::from_response(&response).unwrap()).unwrap();
        assert_eq!(value["id"], "00000000000000aa");
        assert_eq!(value["path"], "/ws");
        assert_eq!(value["op_id"], "ff");
        assert!(Created::from_response(&v0::CreateResponse::default()).is_none());
    }
}
