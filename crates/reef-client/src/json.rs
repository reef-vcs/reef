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
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// A `google.protobuf.Timestamp` as [`rfc3339_millis`] prints it, as the
/// daemon's `Timestamp::to_rfc3339` does (both call it; a daemon test
/// checks the round trip through the message).
pub fn rfc3339(time: &prost_types::Timestamp) -> String {
    rfc3339_millis(millis_of(time))
}

/// Unix milliseconds as RFC 3339 UTC with exactly three fraction digits,
/// such as `2026-10-16T18:31:17.040Z`, so that timestamps sort as strings;
/// `<millis>ms` for an instant RFC 3339 cannot write (outside the years 0
/// to 9999).
pub fn rfc3339_millis(millis: i64) -> String {
    let (seconds, fraction) = (millis.div_euclid(1000), millis.rem_euclid(1000));
    // `time` trims a fraction's trailing zeros, and drops a zero one, so
    // the whole seconds are formatted and the fraction written here.
    OffsetDateTime::from_unix_timestamp(seconds)
        .ok()
        .and_then(|time| time.format(&Rfc3339).ok())
        .and_then(|text| {
            text.strip_suffix('Z')
                .map(|whole| format!("{whole}.{fraction:03}Z"))
        })
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

/// `schema_with` functions for the string fields a name function fills
/// (R2.24): their values listed from the function, so the schema cannot
/// drift from what prints; `null` too where the field prints it.
macro_rules! names_schema {
    ($($schema:ident = $name:ident, $nullable:literal;)*) => {$(
        fn $schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
            crate::schema::names(|value| crate::schema::Name::name($name(value)), $nullable)
        }
    )*};
}

names_schema! {
    attention_name_schema_or_null = attention_name, true;
    backend_name_schema = backend_name, false;
    call_site_status_name_schema_or_null = call_site_status_name, true;
    change_effect_kind_name_schema_or_null = change_effect_kind_name, true;
    change_outcome_name_schema_or_null = change_outcome_name, true;
    entry_type_name_schema_or_null = entry_type_name, true;
    file_change_kind_name_schema_or_null = file_change_kind_name, true;
    graph_edge_kind_name_schema_or_null = graph_edge_kind_name, true;
    history_symbol_coverage_name_schema_or_null = history_symbol_coverage_name, true;
    lease_end_reason_schema_or_null = lease_end_reason, true;
    lease_state_schema = lease_state, false;
    lifecycle_state_name_schema = lifecycle_state_name_or_unknown, false;
    lifecycle_state_name_schema_or_null = lifecycle_state_name, true;
    transition_actor_name_schema_or_null = transition_actor_name, true;
    assignment_state_name_schema_or_null = assignment_state_name, true;
    publication_update_name_schema_or_null = publication_update_name, true;
    git_host_kind_name_schema_or_null = git_host_kind_name, true;
    assignment_reason_name_schema_or_null = assignment_reason_name, true;
    transition_cause_name_schema_or_null = transition_cause_name, true;
    comment_kind_name_schema_or_null = comment_kind_name, true;
    instruction_state_name_schema_or_null = instruction_state_name, true;
    instruction_cause_name_schema_or_null = instruction_cause_name, true;
    operation_kind_name_schema_or_null = operation_kind_name, true;
    presence_kind_schema = presence_kind, false;
    snapshot_kind_name_schema = snapshot_kind_name, false;
    snapshot_kind_opt_schema_or_null = snapshot_kind_opt, true;
    state_name_schema = state_name, false;
    status_source_name_schema_or_null = status_source_name, true;
    status_walk_reason_name_schema = status_walk_reason_name, false;
    symbol_change_kind_name_schema_or_null = symbol_change_kind_name, true;
    symbol_coverage_name_schema_or_null = symbol_coverage_name, true;
    symbol_index_state_name_schema_or_null = symbol_index_state_name, true;
    symbol_kind_name_schema_or_null = symbol_kind_name, true;
    symbol_role_name_schema_or_null = symbol_role_name, true;
    symbol_skip_reason_name_schema_or_null = symbol_skip_reason_name, true;
    symbol_source_name_schema_or_null = symbol_source_name, true;
    touch_state_name_schema_or_null = touch_state_name, true;
    watch_state_name_schema_or_null = watch_state_name, true;
    watcher_name_schema = watcher_name, false;
}

/// `UploadProblem.reason`: a rejection's name, or one of the two the CLI
/// gives a file it did not send.
fn upload_problem_reason_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
    let mut schema = crate::schema::names(|value| Some(scip_rejection_name(value)), false);
    if let Some(serde_json::Value::Array(values)) = schema.get_mut("enum") {
        values.extend(["unreadable", "too_large"].map(serde_json::Value::from));
    }
    schema
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
    #[schemars(schema_with = "lease_state_schema")]
    pub state: String,
    /// `released`, `expired`, or `restored` once it has ended.
    #[schemars(schema_with = "lease_end_reason_schema_or_null")]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
    #[schemars(schema_with = "backend_name_schema")]
    pub backend: String,
    /// Its directory.
    pub path: String,
    /// `active`, `releasing`, `released`, or `reclaimed`.
    #[schemars(schema_with = "state_name_schema")]
    pub state: String,
    /// Whether its change is divergent.
    pub divergent: bool,
    /// Why it needs attention, if it does.
    #[schemars(schema_with = "attention_name_schema_or_null")]
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
    #[schemars(schema_with = "watcher_name_schema")]
    pub watcher: String,
    /// `waiting` while its Watchman subscription waits for a burst of
    /// creates to end (R1.29); `attaching` while its notify watch attaches
    /// in the background (Linux, R1.32).
    #[schemars(schema_with = "watch_state_name_schema_or_null")]
    pub watch_state: Option<String>,
    /// How many of its snapshots have not been published; 0 in `ws create`.
    pub local_snapshot_count: u64,
    /// The shared stores mounted in it (R1.20).
    pub stores: Vec<MountedStore>,
    /// How many paths its change changes as of its last snapshot; only
    /// `ws list` fills it (issue #114).
    pub changed_file_count: Option<u32>,
    /// The remote token that created it (R2.08); absent when the socket
    /// did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_token_id: Option<String>,
    /// Its live provenance session (R3.04): the identity its records are
    /// bound to, and the declared context now. Absent when provenance is
    /// disabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<Box<ProvenanceSession>>,
}

/// A shared store mounted in a workspace (R1.20).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
            changed_file_count: ws.changed_file_count,
            remote_token_id: text(&ws.remote_token_id),
            provenance: ws
                .provenance
                .as_deref()
                .map(|session| Box::new(ProvenanceSession::from(session))),
        }
    }
}

/// One indexed snapshot operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SnapshotEntry {
    /// The operation.
    pub op_id: String,
    /// The working-copy commit it wrote.
    pub commit_id: String,
    /// `watcher`, `explicit`, `final`, `move`, or `shutdown`.
    #[schemars(schema_with = "snapshot_kind_name_schema")]
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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

/// `reef ui`'s answer (R2.18). `session_url`, a credential, only with
/// `--print`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct WebSession {
    /// The web UI's address, without a token.
    pub url: String,
    /// The launch file `reef ui` opens, if it asked for one.
    pub launch_path: Option<String>,
    /// Whether a browser was opened.
    pub opened: bool,
    /// The session URL, `url` with the token in its fragment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_url: Option<String>,
}

/// One materialization backend's probe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct BackendProbe {
    /// The backend's name.
    #[schemars(schema_with = "backend_name_schema")]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
    /// The remote listener (R2.08); absent from a daemon before it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteListener>,
}

/// The remote listener, as `daemon status` and `remote show` print it
/// (R2.08).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RemoteListener {
    /// `off`, `listening` or `failed`; `starting` before its first bind.
    pub state: String,
    /// `config` or `flag`; `null` when off.
    pub enabled_by: Option<String>,
    /// The bound address, or `[remote] listen` until bound.
    pub address: String,
    /// The names it serves.
    pub names: Vec<String>,
    /// The certificate it presents; `null` until loaded.
    pub certificate: Option<RemoteCertificate>,
    /// Why it is failed, or why a reload failed.
    pub error: Option<String>,
    /// Tokens neither revoked nor expired.
    pub live_tokens: u32,
}

/// The remote listener's certificate (R2.08).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RemoteCertificate {
    /// `generated` or `file`.
    pub source: String,
    /// `sha256:<hex>` of its key: what clients pin.
    pub fingerprint: String,
    /// When it expires.
    pub not_after: Option<String>,
    /// Its subject alternative names.
    pub subject_names: Vec<String>,
}

impl From<&v1::RemoteListener> for RemoteListener {
    fn from(remote: &v1::RemoteListener) -> Self {
        let state = match v1::RemoteListenerState::try_from(remote.state) {
            Ok(v1::RemoteListenerState::Off) => "off",
            Ok(v1::RemoteListenerState::Listening) => "listening",
            Ok(v1::RemoteListenerState::Failed) => "failed",
            _ => "starting",
        };
        let enabled_by = match v1::RemoteEnabledBy::try_from(remote.enabled_by) {
            Ok(v1::RemoteEnabledBy::Config) => Some("config"),
            Ok(v1::RemoteEnabledBy::Flag) => Some("flag"),
            _ => None,
        };
        Self {
            state: state.to_owned(),
            enabled_by: enabled_by.map(str::to_owned),
            address: remote.address.clone(),
            names: remote.names.clone(),
            certificate: remote.certificate.as_ref().map(|cert| RemoteCertificate {
                source: match v1::RemoteCertificateSource::try_from(cert.source) {
                    Ok(v1::RemoteCertificateSource::File) => "file",
                    _ => "generated",
                }
                .to_owned(),
                fingerprint: cert.fingerprint.clone(),
                not_after: time(cert.not_after.as_ref()),
                subject_names: cert.subject_names.clone(),
            }),
            error: text(&remote.error),
            live_tokens: remote.live_token_count,
        }
    }
}

/// A remote token, without its secret, as `remote token list` prints it
/// (R2.08).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RemoteToken {
    /// Its id.
    pub id: String,
    /// Its name.
    pub name: String,
    /// `read` or `write`.
    pub scope: String,
    /// `live`, `expired` or `revoked`.
    pub state: String,
    /// When it was issued.
    pub created_at: Option<String>,
    /// When it expires; `null`: never.
    pub expires_at: Option<String>,
    /// When it was last rotated.
    pub rotated_at: Option<String>,
    /// Until when the secret before the last rotation works.
    pub previous_valid_until: Option<String>,
    /// When it was revoked.
    pub revoked_at: Option<String>,
    /// When it was last used, to the minute.
    pub last_used_at: Option<String>,
    /// The address it was last used from.
    pub last_used_from: Option<String>,
    /// The operator identity it is linked to (R3.03): the only one it gets
    /// session keys for; `null` for none.
    pub identity: Option<String>,
}

impl From<&v1::RemoteToken> for RemoteToken {
    fn from(token: &v1::RemoteToken) -> Self {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |since| {
                i64::try_from(since.as_secs()).unwrap_or(i64::MAX)
            });
        let state = if token.revoked_at.is_some() {
            "revoked"
        } else if token
            .expires_at
            .as_ref()
            .is_some_and(|at| at.seconds <= now)
        {
            "expired"
        } else {
            "live"
        };
        Self {
            id: token.id.clone(),
            name: token.name.clone(),
            scope: match v1::RemoteScope::try_from(token.scope) {
                Ok(v1::RemoteScope::Write) => "write",
                _ => "read",
            }
            .to_owned(),
            state: state.to_owned(),
            created_at: time(token.created_at.as_ref()),
            expires_at: time(token.expires_at.as_ref()),
            rotated_at: time(token.rotated_at.as_ref()),
            previous_valid_until: time(token.previous_valid_until.as_ref()),
            revoked_at: time(token.revoked_at.as_ref()),
            last_used_at: time(token.last_used_at.as_ref()),
            last_used_from: text(&token.last_used_from),
            identity: text(&token.identity),
        }
    }
}

/// A token just created or rotated: printed once (R2.08).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CreatedRemoteToken {
    /// The token, without its secret.
    #[serde(flatten)]
    pub token_info: RemoteToken,
    /// The bearer token: a credential, shown once.
    pub token: String,
    /// `https://<name>:<port>`, when the listener is on.
    pub url: Option<String>,
    /// The web UI with the token in its fragment, when the listener is on:
    /// a credential.
    pub web_url: Option<String>,
    /// What clients pin, when the listener has a certificate.
    pub fingerprint: Option<String>,
    /// The command to run on the other machine.
    pub add: Option<Vec<String>>,
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
            remote: status.remote.as_ref().map(RemoteListener::from),
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
            remote: None,
        }
    }
}

/// A built store, from `StoreService.ListStores` (R1.20).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
    /// What they had allocated: an upper bound on what removing them freed.
    pub base_bytes_freed: u64,
    /// What removing them freed on disk, counting blocks they shared once;
    /// `null` where the filesystem cannot tell (#95).
    pub base_bytes_unshared: Option<u64>,
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
    /// Evidence objects deleted: not on live work, and past `[evidence]
    /// retention` (R3.02).
    pub evidence_removed: u32,
    /// Artifacts whose bytes were deleted, a record kept or not.
    pub artifacts_collected: u32,
    /// What their files had allocated; in a dry run, their sizes.
    pub artifact_bytes_freed: u64,
    /// The sizes of the artifacts stored after the step.
    pub artifact_bytes: u64,
    /// Why the evidence and artifacts step stopped short.
    pub evidence_error: Option<String>,
    /// Provenance records compacted away (R3.04).
    pub provenance_compacted: u32,
    /// Provenance records of changes with no visible commit, expired.
    pub provenance_removed: u32,
    /// Why the provenance step stopped short.
    pub provenance_error: Option<String>,
    /// Finished executor jobs deleted (R3.05).
    pub jobs_removed: u32,
    /// Why the jobs step stopped short.
    pub jobs_error: Option<String>,
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
            base_bytes_unshared: report.base_bytes_unshared,
            workspace_rows_removed: report.workspace_rows_removed,
            wc_bytes_freed: report.wc_bytes_freed,
            stores_removed: report.stores_removed,
            store_bytes_freed: report.store_bytes_freed,
            elapsed_ms: report.elapsed_ms,
            dry_run: report.dry_run,
            evidence_removed: report.evidence_removed,
            artifacts_collected: report.artifacts_collected,
            artifact_bytes_freed: report.artifact_bytes_freed,
            artifact_bytes: report.artifact_bytes,
            evidence_error: text(&report.evidence_error),
            provenance_compacted: report.provenance_compacted,
            provenance_removed: report.provenance_removed,
            provenance_error: text(&report.provenance_error),
            jobs_removed: report.jobs_removed,
            jobs_error: text(&report.jobs_error),
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
/// `last-snapshot`, or `operation` (R2.12); `None` when unset.
pub fn status_source_name(source: i32) -> Option<&'static str> {
    let name = match v1::StatusSource::try_from(source) {
        Ok(v1::StatusSource::Unspecified) => return None,
        Ok(v1::StatusSource::Snapshot) => "snapshot",
        Ok(v1::StatusSource::Index) => "index",
        Ok(v1::StatusSource::Walk) => "walk",
        Ok(v1::StatusSource::LastSnapshot) => "last-snapshot",
        Ok(v1::StatusSource::Operation) => "operation",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// Why a status walked the tree instead of reading the index (R2.23):
/// `not-watched`, `watch-restarted`, `events-lost`, `too-broad`,
/// `snapshot-failed`, `not-synced`, `stale-working-copy`,
/// `path-not-narrowable`, or `covered-by-full-scan`; `None` when unset.
pub fn status_walk_reason_name(reason: i32) -> Option<&'static str> {
    let name = match v1::StatusWalkReason::try_from(reason) {
        Ok(v1::StatusWalkReason::Unspecified) => return None,
        Ok(v1::StatusWalkReason::NotWatched) => "not-watched",
        Ok(v1::StatusWalkReason::WatchRestarted) => "watch-restarted",
        Ok(v1::StatusWalkReason::EventsLost) => "events-lost",
        Ok(v1::StatusWalkReason::TooBroad) => "too-broad",
        Ok(v1::StatusWalkReason::SnapshotFailed) => "snapshot-failed",
        Ok(v1::StatusWalkReason::NotSynced) => "not-synced",
        Ok(v1::StatusWalkReason::StaleWorkingCopy) => "stale-working-copy",
        Ok(v1::StatusWalkReason::PathNotNarrowable) => "path-not-narrowable",
        Ok(v1::StatusWalkReason::CoveredByFullScan) => "covered-by-full-scan",
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
        Ok(v1::OperationKind::Transition) => "transition",
        Ok(v1::OperationKind::Land) => "land",
        Ok(v1::OperationKind::Comment) => "comment",
        Ok(v1::OperationKind::Batch) => "batch",
        Ok(v1::OperationKind::ConflictAssignment) => "conflict-assignment",
        Ok(v1::OperationKind::Publish) => "publish",
        Ok(v1::OperationKind::External) => "external",
        Ok(v1::OperationKind::Other) => "other",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// A lifecycle state's name (R3.09): `draft`, `proposed`, `queued`,
/// `landed`, `declined`, `withdrawn` or `superseded`; `None` when unset.
pub fn lifecycle_state_name(state: i32) -> Option<&'static str> {
    let name = match v1::LifecycleState::try_from(state) {
        Ok(v1::LifecycleState::Unspecified) => return None,
        Ok(v1::LifecycleState::Draft) => "draft",
        Ok(v1::LifecycleState::Proposed) => "proposed",
        Ok(v1::LifecycleState::Queued) => "queued",
        Ok(v1::LifecycleState::Landed) => "landed",
        Ok(v1::LifecycleState::Declined) => "declined",
        Ok(v1::LifecycleState::Withdrawn) => "withdrawn",
        Ok(v1::LifecycleState::Superseded) => "superseded",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// [`lifecycle_state_name`], with `unknown` for unset.
pub fn lifecycle_state_name_or_unknown(state: i32) -> &'static str {
    lifecycle_state_name(state).unwrap_or(UNKNOWN)
}

/// A transition's cause (R3.09): `request`, `undo`, `observed` or
/// `landing` (R3.13); `None`
/// when unset.
pub fn transition_cause_name(cause: i32) -> Option<&'static str> {
    let name = match v1::TransitionCause::try_from(cause) {
        Ok(v1::TransitionCause::Unspecified) => return None,
        Ok(v1::TransitionCause::Request) => "request",
        Ok(v1::TransitionCause::Undo) => "undo",
        Ok(v1::TransitionCause::Observed) => "observed",
        Ok(v1::TransitionCause::Landing) => "landing",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// A comment's kind (R3.12): `note` or `instruction`; `None` when unset.
pub fn comment_kind_name(kind: i32) -> Option<&'static str> {
    let name = match v1::CommentKind::try_from(kind) {
        Ok(v1::CommentKind::Unspecified) => return None,
        Ok(v1::CommentKind::Note) => "note",
        Ok(v1::CommentKind::Instruction) => "instruction",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// An instruction's state (R3.12): `awaiting_confirmation`, `pending`,
/// `delivered`, `running`, `done`, `failed` or `cancelled`; `None` when
/// unset.
pub fn instruction_state_name(state: i32) -> Option<&'static str> {
    let name = match v1::InstructionState::try_from(state) {
        Ok(v1::InstructionState::Unspecified) => return None,
        Ok(v1::InstructionState::AwaitingConfirmation) => "awaiting_confirmation",
        Ok(v1::InstructionState::Pending) => "pending",
        Ok(v1::InstructionState::Delivered) => "delivered",
        Ok(v1::InstructionState::Running) => "running",
        Ok(v1::InstructionState::Done) => "done",
        Ok(v1::InstructionState::Failed) => "failed",
        Ok(v1::InstructionState::Cancelled) => "cancelled",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// Why an instruction moved (R3.12): `request`, `reproposed`,
/// `change_final` or `expired`; `None` when unset.
pub fn instruction_cause_name(cause: i32) -> Option<&'static str> {
    let name = match v1::InstructionCause::try_from(cause) {
        Ok(v1::InstructionCause::Unspecified) => return None,
        Ok(v1::InstructionCause::Request) => "request",
        Ok(v1::InstructionCause::Reproposed) => "reproposed",
        Ok(v1::InstructionCause::ChangeFinal) => "change_final",
        Ok(v1::InstructionCause::Expired) => "expired",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// Who performed a transition (R3.09): `human`, `agent` or `daemon`;
/// `None` when unset.
pub fn transition_actor_name(actor: i32) -> Option<&'static str> {
    let name = match v1::TransitionActor::try_from(actor) {
        Ok(v1::TransitionActor::Unspecified) => return None,
        Ok(v1::TransitionActor::Human) => "human",
        Ok(v1::TransitionActor::Agent) => "agent",
        Ok(v1::TransitionActor::Daemon) => "daemon",
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

/// A resolver assignment's state (R3.27): `assigned`, `queued`, `taken`,
/// `unowned` or `closed`; `None` when unset.
pub fn assignment_state_name(state: i32) -> Option<&'static str> {
    let name = match v1::ConflictAssignmentState::try_from(state) {
        Ok(v1::ConflictAssignmentState::Unspecified) => return None,
        Ok(v1::ConflictAssignmentState::Assigned) => "assigned",
        Ok(v1::ConflictAssignmentState::Queued) => "queued",
        Ok(v1::ConflictAssignmentState::Taken) => "taken",
        Ok(v1::ConflictAssignmentState::Unowned) => "unowned",
        Ok(v1::ConflictAssignmentState::Closed) => "closed",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// Why a resolver assignment is where it is (R3.27): `default`,
/// `followed`, `returned`, `owner-ended`, `no-owner`, `claimed`,
/// `released`, `assigned`, `taken`, `changed`, `resolved`, `inherited`,
/// `abandoned`, `immutable` or `reopened`; `None` when unset.
pub fn assignment_reason_name(reason: i32) -> Option<&'static str> {
    use v1::ConflictAssignmentReason as R;
    let name = match R::try_from(reason) {
        Ok(R::Unspecified) => return None,
        Ok(R::Default) => "default",
        Ok(R::Followed) => "followed",
        Ok(R::Returned) => "returned",
        Ok(R::OwnerEnded) => "owner-ended",
        Ok(R::NoOwner) => "no-owner",
        Ok(R::Claimed) => "claimed",
        Ok(R::Released) => "released",
        Ok(R::Assigned) => "assigned",
        Ok(R::Taken) => "taken",
        Ok(R::Changed) => "changed",
        Ok(R::Resolved) => "resolved",
        Ok(R::Inherited) => "inherited",
        Ok(R::Abandoned) => "abandoned",
        Ok(R::Immutable) => "immutable",
        Ok(R::Reopened) => "reopened",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// Who owns a resolver assignment (R3.27): a provenance session and its
/// workspace, or a person's identity alone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ConflictOwner {
    /// The provenance session; `null` for a person.
    pub session_id: Option<String>,
    /// Its workspace.
    pub workspace_id: Option<String>,
    /// That workspace's owner.
    pub workspace_owner: Option<String>,
    /// The operator identity's name, else its id.
    pub identity: Option<String>,
    /// `human`, `agent` or `ci`.
    pub actor: Option<String>,
    /// The agent label its session declared; untrusted text.
    pub agent: Option<String>,
    /// The client session its session declared; untrusted text.
    pub client_session: Option<String>,
    /// Whether its session is live.
    pub live: bool,
    /// When its session ended.
    pub ended_at: Option<String>,
}

impl From<&v1::ConflictOwner> for ConflictOwner {
    fn from(owner: &v1::ConflictOwner) -> Self {
        Self {
            session_id: text(&owner.session_id),
            workspace_id: text(&owner.workspace_id),
            workspace_owner: text(&owner.workspace_owner),
            identity: text(&owner.identity_name).or_else(|| text(&owner.identity_id)),
            actor: text(&owner.actor),
            agent: text(&owner.agent),
            client_session: text(&owner.client_session),
            live: owner.live,
            ended_at: time(owner.ended_at.as_ref()),
        }
    }
}

/// A resolver assignment (R3.27): one change that introduces a conflict,
/// and who should resolve it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ConflictAssignment {
    /// `asg_` and 16 hex digits.
    pub id: String,
    /// The change, in hex.
    pub change_id: String,
    /// The same, in jj's reverse hex.
    pub change_id_reverse_hex: String,
    /// Its conflicted commit, as last looked at.
    pub commit_id: String,
    /// The conflict's id, as last looked at.
    pub conflict_id: String,
    /// The conflict's id when the assignment opened.
    pub first_conflict_id: String,
    /// Conflicted files the change introduces.
    pub introduced: u32,
    /// Conflicted files in all.
    pub files_total: u32,
    /// Where it stands.
    #[schemars(schema_with = "assignment_state_name_schema_or_null")]
    pub state: Option<String>,
    /// Why.
    #[schemars(schema_with = "assignment_reason_name_schema_or_null")]
    pub reason: Option<String>,
    /// Who owns it; for `queued`, its last owner.
    pub owner: Option<ConflictOwner>,
    /// From -100 to 100.
    pub priority: i32,
    /// When it last entered the queue.
    pub queued_at: Option<String>,
    /// Its place in the queue, from 1; `null` when not queued or not
    /// listed.
    pub queue_position: Option<u32>,
    /// When it will be queued unless its ended owner is followed.
    pub queue_after: Option<String>,
    /// Claims made on it.
    pub claims: u32,
    /// Events it has.
    pub events: u32,
    /// When it opened.
    pub opened_at: Option<String>,
    /// When it last changed.
    pub updated_at: Option<String>,
    /// When it closed.
    pub closed_at: Option<String>,
    /// The change's commit when it closed.
    pub closed_commit_id: Option<String>,
    /// The session that recorded that commit, when the daemon did.
    pub resolved_session_id: Option<String>,
}

impl From<&v1::ConflictAssignment> for ConflictAssignment {
    fn from(row: &v1::ConflictAssignment) -> Self {
        Self {
            id: row.id.clone(),
            change_id: row.change_id.clone(),
            change_id_reverse_hex: reverse_hex(&row.change_id),
            commit_id: row.commit_id.clone(),
            conflict_id: row.conflict_id.clone(),
            first_conflict_id: row.first_conflict_id.clone(),
            introduced: row.introduced,
            files_total: row.files_total,
            state: name(assignment_state_name(row.state)),
            reason: name(assignment_reason_name(row.reason)),
            owner: row.owner.as_ref().map(ConflictOwner::from),
            priority: row.priority,
            queued_at: time(row.queued_at.as_ref()),
            queue_position: (row.queue_position > 0).then_some(row.queue_position),
            queue_after: time(row.queue_after.as_ref()),
            claims: row.claims,
            events: row.events,
            opened_at: time(row.opened_at.as_ref()),
            updated_at: time(row.updated_at.as_ref()),
            closed_at: time(row.closed_at.as_ref()),
            closed_commit_id: text(&row.closed_commit_id),
            resolved_session_id: text(&row.resolved_session_id),
        }
    }
}

/// One change of a resolver assignment (R3.27).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ConflictAssignmentEvent {
    /// `cae_` and 16 hex digits.
    pub id: String,
    /// Its place in the repository's events.
    pub seq: u64,
    /// The conflict's id then.
    pub conflict_id: String,
    /// The state before; `null` when it opened.
    #[schemars(schema_with = "assignment_state_name_schema_or_null")]
    pub from: Option<String>,
    /// The state after.
    #[schemars(schema_with = "assignment_state_name_schema_or_null")]
    pub to: Option<String>,
    /// Why.
    #[schemars(schema_with = "assignment_reason_name_schema_or_null")]
    pub reason: Option<String>,
    /// The owner afterwards.
    pub owner: Option<ConflictOwner>,
    /// The priority afterwards.
    pub priority: i32,
    /// The reason the caller gave; untrusted text.
    pub note: Option<String>,
    /// `human`, `agent` or `daemon`.
    #[schemars(schema_with = "transition_actor_name_schema_or_null")]
    pub by_actor: Option<String>,
    /// The caller's agent; untrusted text.
    pub by_agent: Option<String>,
    /// The caller's identity.
    pub by_identity_id: Option<String>,
    /// The remote token the call came through.
    pub by_remote_token: Option<String>,
    /// When.
    pub at: Option<String>,
}

impl From<&v1::ConflictAssignmentEvent> for ConflictAssignmentEvent {
    fn from(event: &v1::ConflictAssignmentEvent) -> Self {
        Self {
            id: event.id.clone(),
            seq: event.seq,
            conflict_id: event.conflict_id.clone(),
            from: name(assignment_state_name(event.from_state)),
            to: name(assignment_state_name(event.to_state)),
            reason: name(assignment_reason_name(event.reason)),
            owner: event.owner.as_ref().map(ConflictOwner::from),
            priority: event.priority,
            note: text(&event.note),
            by_actor: name(transition_actor_name(event.by_actor)),
            by_agent: text(&event.by_agent),
            by_identity_id: text(&event.by_identity_id),
            by_remote_token: text(&event.by_remote_token),
            at: time(event.at.as_ref()),
        }
    }
}

/// An author or committer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
    /// Its lifecycle (R3.09); `null` when the daemon did not say.
    pub lifecycle: Option<ChangeLifecycle>,
}

/// A change's lifecycle (R3.09, `docs/proposals.md`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ChangeLifecycle {
    /// `draft`, `proposed`, `queued`, `landed`, `declined`, `withdrawn` or
    /// `superseded`.
    #[schemars(schema_with = "lifecycle_state_name_schema")]
    pub state: String,
    /// Whether a transition was recorded; false for a derived `draft`, or a
    /// derived `landed` (immutable, never proposed through Reef).
    pub recorded: bool,
    /// Its proposal round; 0 when not recorded.
    pub round: u32,
    /// The commit its latest transition recorded.
    pub commit_id: Option<String>,
    /// While proposed or queued: its visible commit is another one.
    pub amended: bool,
    /// The bookmark it is proposed for.
    pub destination: Option<String>,
    /// For a superseded change, the superseding change in hex.
    pub superseded_by: Option<String>,
    /// Its latest transition.
    pub last: Option<ChangeTransition>,
    /// Whether the change has a visible commit.
    pub visible: bool,
}

impl From<&v1::ChangeLifecycle> for ChangeLifecycle {
    fn from(lifecycle: &v1::ChangeLifecycle) -> Self {
        Self {
            state: lifecycle_state_name_or_unknown(lifecycle.state).to_owned(),
            recorded: lifecycle.recorded,
            round: lifecycle.round,
            commit_id: text(&lifecycle.commit_id),
            amended: lifecycle.amended,
            destination: text(&lifecycle.destination),
            superseded_by: text(&lifecycle.superseded_by),
            last: lifecycle.last.as_ref().map(ChangeTransition::from),
            visible: lifecycle.visible,
        }
    }
}

/// One recorded transition of a change's lifecycle (R3.09).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ChangeTransition {
    /// `trn_` and 16 hex digits.
    pub id: String,
    /// The change, in hex.
    pub change_id: String,
    /// The same, in jj's reverse hex.
    pub change_id_reverse_hex: String,
    /// Its commit then.
    pub commit_id: String,
    /// The state before.
    #[schemars(schema_with = "lifecycle_state_name_schema_or_null")]
    pub from: Option<String>,
    /// The state after.
    #[schemars(schema_with = "lifecycle_state_name_schema_or_null")]
    pub to: Option<String>,
    /// `request`, `undo` or `observed`.
    #[schemars(schema_with = "transition_cause_name_schema_or_null")]
    pub cause: Option<String>,
    /// Its proposal round.
    pub round: u32,
    /// The reason given; untrusted text.
    pub reason: Option<String>,
    /// For a supersede, the superseding change.
    pub superseded_by: Option<String>,
    /// The bookmark the change is proposed for.
    pub destination: Option<String>,
    /// For an undo, the operation it reverts.
    pub undoes_operation_id: Option<String>,
    /// The operation that recorded it.
    pub operation_id: Option<String>,
    /// GC collected that operation.
    pub operation_collected: bool,
    /// When it was recorded.
    pub recorded_at: Option<String>,
    /// The call's agent.
    pub agent: Option<String>,
    /// `human`, `agent` or `daemon`, as recorded.
    #[schemars(schema_with = "transition_actor_name_schema_or_null")]
    pub actor: Option<String>,
    /// The operator identity recorded.
    pub identity: Option<String>,
    /// The provenance session of the workspace the call named.
    pub session_id: Option<String>,
    /// The remote token the call came through.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteCaller>,
}

impl From<&v1::ChangeTransition> for ChangeTransition {
    fn from(transition: &v1::ChangeTransition) -> Self {
        Self {
            id: transition.id.clone(),
            change_id: transition.change_id.clone(),
            change_id_reverse_hex: reverse_hex(&transition.change_id),
            commit_id: transition.commit_id.clone(),
            from: name(lifecycle_state_name(transition.from)),
            to: name(lifecycle_state_name(transition.to)),
            cause: name(transition_cause_name(transition.cause)),
            round: transition.round,
            reason: text(&transition.reason),
            superseded_by: text(&transition.superseded_by),
            destination: text(&transition.destination),
            undoes_operation_id: text(&transition.undoes_operation_id),
            operation_id: text(&transition.operation_id),
            operation_collected: transition.operation_collected,
            recorded_at: time(transition.recorded_at.as_ref()),
            agent: text(&transition.agent),
            actor: name(transition_actor_name(transition.actor)),
            identity: text(&transition.identity),
            session_id: text(&transition.session_id),
            remote: transition.remote.as_ref().map(|remote| RemoteCaller {
                token_id: remote.token_id.clone(),
                token_name: remote.token_name.clone(),
            }),
        }
    }
}

/// `reef propose` and `reef proposal withdraw|decline|supersede`: what a
/// transition recorded (R3.09).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Transitioned {
    /// The change afterwards.
    pub change: Change,
    /// Its lifecycle afterwards.
    pub lifecycle: Option<ChangeLifecycle>,
    /// The transition recorded; `null` when nothing changed.
    pub transition: Option<ChangeTransition>,
    /// The operation that recorded it; `null` when nothing changed.
    pub operation_id: Option<String>,
    /// The snapshot taken first, if it recorded one.
    pub snapshot_operation_id: Option<String>,
}

impl From<&v1::TransitionChangeResponse> for Transitioned {
    fn from(response: &v1::TransitionChangeResponse) -> Self {
        Self {
            change: change_or_default(response.change.as_ref()),
            lifecycle: response.lifecycle.as_ref().map(ChangeLifecycle::from),
            transition: response.transition.as_ref().map(ChangeTransition::from),
            operation_id: operation_id(response.operation.as_ref()),
            snapshot_operation_id: operation_id(response.snapshot.as_ref()),
        }
    }
}

/// `reef proposal list`: recorded lifecycles (R3.09).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Proposals {
    /// The repository.
    pub repo_id: String,
    /// Newest update first.
    pub proposals: Vec<Proposal>,
}

/// One recorded lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Proposal {
    /// The change, in hex.
    pub change_id: String,
    /// The same, in jj's reverse hex.
    pub change_id_reverse_hex: String,
    /// Its lifecycle.
    pub lifecycle: Option<ChangeLifecycle>,
    /// The change at head; `null` when it has no visible commit.
    pub change: Option<Change>,
    /// The agent of its latest proposal.
    pub proposed_by: Option<String>,
}

impl From<&v1::ProposalEntry> for Proposal {
    fn from(entry: &v1::ProposalEntry) -> Self {
        Self {
            change_id: entry.change_id.clone(),
            change_id_reverse_hex: reverse_hex(&entry.change_id),
            lifecycle: entry.lifecycle.as_ref().map(ChangeLifecycle::from),
            change: entry.change.as_ref().map(Change::from),
            proposed_by: text(&entry.proposed_by),
        }
    }
}

/// `reef proposal show`: a change's lifecycle and its transitions (R3.09).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProposalHistory {
    /// The change.
    pub change: Change,
    /// Newest first.
    pub transitions: Vec<ChangeTransition>,
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
            lifecycle: change.lifecycle.as_ref().map(ChangeLifecycle::from),
        }
    }
}

fn change_or_default(change: Option<&v1::Change>) -> Change {
    change.map_or_else(|| Change::from(&v1::Change::default()), Change::from)
}

/// An edge of the log's graph, from an entry to an ancestor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct GraphEdge {
    /// The ancestor's commit.
    pub commit_id: String,
    /// `direct`, `indirect`, or `missing`.
    #[schemars(schema_with = "graph_edge_kind_name_schema_or_null")]
    pub kind: Option<String>,
}

/// One entry of `reef log`: a change and its graph edges.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
    #[schemars(schema_with = "operation_kind_name_schema_or_null")]
    pub kind: Option<String>,
    /// For a snapshot: `watcher`, `explicit`, `final`, `move`, or
    /// `shutdown`.
    #[schemars(schema_with = "snapshot_kind_opt_schema_or_null")]
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
    /// The remote token whose call caused it (R2.08); absent for calls
    /// over the socket.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote: Option<RemoteCaller>,
    /// The lifecycle transitions it recorded (R3.09); absent for none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub transitions: Vec<ChangeTransition>,
    /// The comment events it recorded (R3.12); absent for none.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<CommentEvent>,
}

/// One comment write an operation recorded (R3.12): ids and states, never
/// the text, which `reef comment show` reads.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CommentEvent {
    /// `cev_` and 16 hex digits.
    pub id: String,
    /// The comment, `cmt_` and 16 hex digits.
    pub comment_id: String,
    /// Its change, in hex.
    pub change_id: String,
    /// The same, in jj's reverse hex.
    pub change_id_reverse_hex: String,
    /// The change's proposal round when the comment was written.
    pub round: u32,
    /// `note` or `instruction`.
    #[schemars(schema_with = "comment_kind_name_schema_or_null")]
    pub kind: Option<String>,
    /// `comment`, `instruct`, `confirm`, `claim`, `start`, `finish`,
    /// `cancel` or `release`.
    pub event: String,
    /// An instruction's state before.
    #[schemars(schema_with = "instruction_state_name_schema_or_null")]
    pub from: Option<String>,
    /// Its state after.
    #[schemars(schema_with = "instruction_state_name_schema_or_null")]
    pub to: Option<String>,
    /// Why.
    #[schemars(schema_with = "instruction_cause_name_schema_or_null")]
    pub cause: Option<String>,
}

impl From<&v1::CommentEvent> for CommentEvent {
    fn from(event: &v1::CommentEvent) -> Self {
        Self {
            id: event.id.clone(),
            comment_id: event.comment_id.clone(),
            change_id: event.change_id.clone(),
            change_id_reverse_hex: reverse_hex(&event.change_id),
            round: event.round,
            kind: name(comment_kind_name(event.kind)),
            event: event.event.clone(),
            from: name(instruction_state_name(event.from)),
            to: name(instruction_state_name(event.to)),
            cause: name(instruction_cause_name(event.cause)),
        }
    }
}

/// Who called through the remote listener (R2.08).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct RemoteCaller {
    /// The token's id.
    pub token_id: String,
    /// Its name then.
    pub token_name: String,
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
            remote: op.remote.as_ref().map(|remote| RemoteCaller {
                token_id: remote.token_id.clone(),
                token_name: remote.token_name.clone(),
            }),
            transitions: op.transitions.iter().map(ChangeTransition::from).collect(),
            comments: op.comments.iter().map(CommentEvent::from).collect(),
        }
    }
}

/// What an operation changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ChangeEffect {
    /// The change, in hex.
    pub change_id: String,
    /// The same, in reverse hex.
    pub change_id_reverse_hex: String,
    /// `created`, `rewritten`, `abandoned`, or `hidden`.
    #[schemars(schema_with = "change_effect_kind_name_schema_or_null")]
    pub kind: Option<String>,
    /// Its commit afterwards.
    pub commit_id: Option<String>,
    /// Its commit before.
    pub previous_commit_id: Option<String>,
}

/// What an operation did to one bookmark.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct FileChange {
    /// Repository-relative, `/`-separated.
    pub path: String,
    /// `added`, `modified`, `deleted`, `renamed`, or `copied`.
    #[schemars(schema_with = "file_change_kind_name_schema_or_null")]
    pub kind: Option<String>,
    /// Where a rename or copy came from.
    pub old_path: Option<String>,
    /// The type before; `null` when added.
    #[schemars(schema_with = "entry_type_name_schema_or_null")]
    pub old_type: Option<String>,
    /// The type after; `null` when deleted.
    #[schemars(schema_with = "entry_type_name_schema_or_null")]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
    /// Its symbol changes, with `--symbols` (R2.15); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbols: Option<FileSymbolDiff>,
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
            symbols: diff.symbols.as_ref().map(FileSymbolDiff::from),
        }
    }
}

/// One call site of a changed symbol (R2.15): compact, since the change
/// names the symbol, its kind and the role.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CallSite {
    /// The file.
    pub path: String,
    /// The reference's range.
    pub range: Option<SourceRange>,
    /// The definition containing it: the caller.
    pub caller: Option<String>,
    /// `scip` (by exact symbol) or `tags` (by name).
    #[schemars(schema_with = "symbol_source_name_schema_or_null")]
    pub source: Option<String>,
    /// SCIP's roles beyond the reference, as in `reef symbols list`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<&'static str>,
}

impl From<&v1::SymbolOccurrence> for CallSite {
    fn from(o: &v1::SymbolOccurrence) -> Self {
        let full = SymbolOccurrence::from(o);
        Self {
            path: full.path,
            range: full.range,
            caller: full.enclosing_symbol,
            source: full.source,
            flags: full.flags,
        }
    }
}

/// One definition that differs (R2.15).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SymbolChange {
    /// `added`, `removed`, `changed` or `moved`.
    #[schemars(schema_with = "symbol_change_kind_name_schema_or_null")]
    pub kind: Option<String>,
    /// Its symbol: the new side's, or the old side's when removed.
    pub symbol: String,
    /// The old side's symbol, when it differs.
    pub old_symbol: Option<String>,
    /// Its display name.
    pub name: String,
    /// What it is.
    #[schemars(schema_with = "symbol_kind_name_schema_or_null")]
    pub symbol_kind: Option<String>,
    /// The definition containing it.
    pub enclosing_symbol: Option<String>,
    /// The old definition's file, when another than this file's.
    pub old_path: Option<String>,
    /// The old definition's range.
    pub old_range: Option<SourceRange>,
    /// The new definition's range.
    pub new_range: Option<SourceRange>,
    /// Lines added within it.
    pub lines_added: u32,
    /// Lines removed within it.
    pub lines_removed: u32,
    /// Changed only inside its nested definitions.
    pub nested_only: bool,
    /// `found`, `capped`, `not_tracked`, `index_incomplete` or
    /// `page_limit`; `null` when not looked up.
    #[schemars(schema_with = "call_site_status_name_schema_or_null")]
    pub call_site_status: Option<String>,
    /// Call sites found.
    pub call_site_count: u32,
    /// Some of them.
    pub call_sites: Vec<CallSite>,
    /// Definitions of its name in the new tree, with call sites by name.
    pub name_definitions: u32,
}

fn symbol_change_kind_name(kind: i32) -> Option<&'static str> {
    use v1::SymbolChangeKind as K;
    let name = match K::try_from(kind) {
        Ok(K::Unspecified) => return None,
        Ok(K::Added) => "added",
        Ok(K::Removed) => "removed",
        Ok(K::Changed) => "changed",
        Ok(K::Moved) => "moved",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

fn call_site_status_name(status: i32) -> Option<&'static str> {
    use v1::CallSiteStatus as S;
    let name = match S::try_from(status) {
        Ok(S::Unspecified) => return None,
        Ok(S::Found) => "found",
        Ok(S::Capped) => "capped",
        Ok(S::NotTracked) => "not_tracked",
        Ok(S::IndexIncomplete) => "index_incomplete",
        Ok(S::PageLimit) => "page_limit",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// A file's symbol coverage as JSON names it.
pub fn symbol_coverage_name(coverage: i32) -> Option<&'static str> {
    use v1::SymbolCoverage as C;
    let name = match C::try_from(coverage) {
        Ok(C::Unspecified) => return None,
        Ok(C::Covered) => "covered",
        Ok(C::Unsupported) => "unsupported",
        Ok(C::Skipped) => "skipped",
        Ok(C::NoScip) => "no_scip",
        Ok(C::DiffTooLarge) => "diff_too_large",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

impl From<&v1::SymbolChange> for SymbolChange {
    fn from(c: &v1::SymbolChange) -> Self {
        Self {
            kind: name(symbol_change_kind_name(c.kind)),
            symbol: c.symbol.clone(),
            old_symbol: text(&c.old_symbol),
            name: c.name.clone(),
            symbol_kind: name(symbol_kind_name(c.symbol_kind)),
            enclosing_symbol: text(&c.enclosing_symbol),
            old_path: text(&c.old_path),
            old_range: source_range(c.old_range.as_ref()),
            new_range: source_range(c.new_range.as_ref()),
            lines_added: c.lines_added,
            lines_removed: c.lines_removed,
            nested_only: c.nested_only,
            call_site_status: name(call_site_status_name(c.call_site_status)),
            call_site_count: c.call_site_count,
            call_sites: c.call_sites.iter().map(CallSite::from).collect(),
            name_definitions: c.name_definitions,
        }
    }
}

/// One file's symbol changes (R2.15).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct FileSymbolDiff {
    /// `covered`, `unsupported`, `skipped`, `no_scip` or `diff_too_large`.
    #[schemars(schema_with = "symbol_coverage_name_schema_or_null")]
    pub coverage: Option<String>,
    /// Why a side was skipped.
    #[schemars(schema_with = "symbol_skip_reason_name_schema_or_null")]
    pub skip_reason: Option<String>,
    /// `tags` or `scip`, when covered.
    #[schemars(schema_with = "symbol_source_name_schema_or_null")]
    pub source: Option<String>,
    /// A side parsed with syntax errors.
    pub parse_errors: bool,
    /// The changes.
    pub changes: Vec<SymbolChange>,
    /// More than 1,000 changes; the summary counts them all.
    pub truncated: bool,
}

impl From<&v1::FileSymbolDiff> for FileSymbolDiff {
    fn from(f: &v1::FileSymbolDiff) -> Self {
        Self {
            coverage: name(symbol_coverage_name(f.coverage)),
            skip_reason: (f.skip_reason != 0)
                .then(|| symbol_skip_reason_name(f.skip_reason).to_owned()),
            source: name(symbol_source_name(f.source)),
            parse_errors: f.parse_errors,
            changes: f.changes.iter().map(SymbolChange::from).collect(),
            truncated: f.truncated,
        }
    }
}

/// A diff's symbol changes over every page (R2.15).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SymbolDiffSummary {
    /// Definitions added.
    pub added: u32,
    /// Definitions removed.
    pub removed: u32,
    /// Definitions changed.
    pub changed: u32,
    /// Definitions moved.
    pub moved: u32,
    /// Files covered.
    pub files_covered: u32,
    /// Files no source reads.
    pub files_unsupported: u32,
    /// Files skipped, or without SCIP when it was asked for.
    pub files_skipped: u32,
    /// More than 1,000 files differ: nothing was computed.
    pub diff_too_large: bool,
    /// The new tree's indexing, when call sites were looked up.
    pub to_index: Option<SymbolStatus>,
}

impl From<&v1::SymbolDiffSummary> for SymbolDiffSummary {
    fn from(s: &v1::SymbolDiffSummary) -> Self {
        Self {
            added: s.added,
            removed: s.removed,
            changed: s.changed,
            moved: s.moved,
            files_covered: s.files_covered,
            files_unsupported: s.files_unsupported,
            files_skipped: s.files_skipped,
            diff_too_large: s.diff_too_large,
            to_index: s.to_index.as_ref().map(SymbolStatus::from),
        }
    }
}

/// `reef diff`: every file of a diff between two trees.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
    /// The symbol changes, with `--symbols` (R2.15); absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol_summary: Option<SymbolDiffSummary>,
}

impl From<&v1::GetDiffResponse> for Diff {
    fn from(diff: &v1::GetDiffResponse) -> Self {
        Self {
            from_commit_ids: diff.from_commit_ids.clone(),
            to_commit_id: diff.to_commit_id.clone(),
            at_operation: diff.at_operation.clone(),
            total_files: diff.total_files,
            files: diff.files.iter().map(FileDiff::from).collect(),
            symbol_summary: diff.symbol_summary.as_ref().map(SymbolDiffSummary::from),
        }
    }
}

/// `reef show`: a change and the paths it changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
    #[schemars(schema_with = "status_source_name_schema_or_null")]
    pub source: Option<String>,
    /// Why the tree was walked, with `walk` (R2.23); left out otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(schema_with = "status_walk_reason_name_schema")]
    pub walk_reason: Option<String>,
    /// The snapshot the status took, if it recorded one.
    pub snapshot: Option<Operation>,
    /// The open resolver assignments its session owns (R3.27), at most 10.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflict_assignments: Vec<ConflictAssignment>,
    /// How many.
    #[serde(skip_serializing_if = "is_zero_u32")]
    pub conflict_assignment_count: u32,
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if"
)]
fn is_zero_u32(n: &u32) -> bool {
    *n == 0
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
                walk_reason: name(status_walk_reason_name(ws.walk_reason)),
                snapshot: operation(ws.snapshot.as_ref()),
                conflict_assignments: ws
                    .conflict_assignments
                    .iter()
                    .map(ConflictAssignment::from)
                    .collect(),
                conflict_assignment_count: ws.conflict_assignment_count,
            }),
        }
    }
}

/// `reef describe`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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

/// How a publish moved its branch (#258): `created`, `fast-forward`,
/// `rewritten`, `forced` or `up-to-date`; `None` when unset.
pub fn publication_update_name(update: i32) -> Option<&'static str> {
    let name = match v1::PublicationUpdate::try_from(update) {
        Ok(v1::PublicationUpdate::Unspecified) => return None,
        Ok(v1::PublicationUpdate::Created) => "created",
        Ok(v1::PublicationUpdate::FastForward) => "fast-forward",
        Ok(v1::PublicationUpdate::Rewritten) => "rewritten",
        Ok(v1::PublicationUpdate::Forced) => "forced",
        Ok(v1::PublicationUpdate::UpToDate) => "up-to-date",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// A git host Reef knows (#258): `github`, `gitlab`, `bitbucket` or
/// `forgejo`; `None` when unset.
pub fn git_host_kind_name(kind: i32) -> Option<&'static str> {
    let name = match v1::GitHostKind::try_from(kind) {
        Ok(v1::GitHostKind::Unspecified) => return None,
        Ok(v1::GitHostKind::Github) => "github",
        Ok(v1::GitHostKind::Gitlab) => "gitlab",
        Ok(v1::GitHostKind::Bitbucket) => "bitbucket",
        Ok(v1::GitHostKind::Forgejo) => "forgejo",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// The command and page that open a pull request for a published branch
/// (#258).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PullRequestHint {
    /// `github`, `gitlab`, `bitbucket` or `forgejo`.
    #[schemars(schema_with = "git_host_kind_name_schema_or_null")]
    pub host_kind: Option<String>,
    /// The host, without user information.
    pub host: String,
    /// The repository's path on the host, such as `owner/name`.
    pub repository: String,
    /// The branch the pull request targets; empty when unknown.
    pub base: String,
    /// The host CLI's command that opens it; empty when the host has none.
    pub argv: Vec<String>,
    /// The page that opens it; empty when none.
    pub url: String,
}

impl From<&v1::PullRequestHint> for PullRequestHint {
    fn from(hint: &v1::PullRequestHint) -> Self {
        Self {
            host_kind: name(git_host_kind_name(hint.host_kind)),
            host: hint.host.clone(),
            repository: hint.repository.clone(),
            base: hint.base.clone(),
            argv: hint.argv.clone(),
            url: hint.url.clone(),
        }
    }
}

/// A change published to a branch of a git remote (#258).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Publication {
    /// The git remote.
    pub git_remote: String,
    /// The branch on it.
    pub branch: String,
    /// The change published, in hex.
    pub change_id: String,
    /// The commit pushed.
    pub commit_id: String,
    /// The branch's commit before; `null` when the publish created it.
    pub previous_commit_id: Option<String>,
    /// `created`, `fast-forward`, `rewritten`, `forced` or `up-to-date`.
    #[schemars(schema_with = "publication_update_name_schema_or_null")]
    pub update: Option<String>,
    /// The commits the remote's branches lacked, newest first, at most 100.
    pub commit_ids: Vec<String>,
    /// How many commits that was.
    pub commit_count: u32,
    /// Their changes, newest first, at most 100.
    pub change_ids: Vec<String>,
    /// How to open a pull request; `null` for a host Reef does not know.
    pub pull_request: Option<PullRequestHint>,
    /// The remote's own messages, as it sent them, sanitized.
    pub remote_messages: Vec<String>,
    /// Snapshot rows marked as having left the machine.
    pub local_snapshots_published: u32,
    /// Planned only: nothing was pushed or recorded.
    pub dry_run: bool,
}

impl From<&v1::Publication> for Publication {
    fn from(publication: &v1::Publication) -> Self {
        Self {
            git_remote: publication.git_remote.clone(),
            branch: publication.branch.clone(),
            change_id: publication.change_id.clone(),
            commit_id: publication.commit_id.clone(),
            previous_commit_id: (!publication.previous_commit_id.is_empty())
                .then(|| publication.previous_commit_id.clone()),
            update: name(publication_update_name(publication.update)),
            commit_ids: publication.commit_ids.clone(),
            commit_count: publication.commit_count,
            change_ids: publication.change_ids.clone(),
            pull_request: publication.pull_request.as_ref().map(PullRequestHint::from),
            remote_messages: publication.remote_messages.clone(),
            local_snapshots_published: publication.local_snapshots_published,
            dry_run: publication.dry_run,
        }
    }
}

/// `reef publish`'s answer (#258).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Published {
    /// What was published, and where.
    pub publication: Publication,
    /// The publish; `null` for a dry run or when nothing changed.
    pub operation: Option<Operation>,
    /// The snapshot taken first, if it recorded one.
    pub snapshot: Option<Operation>,
    /// The describe run first, if it recorded one.
    pub describe: Option<Operation>,
    /// The change published.
    pub change: Option<Change>,
    /// The scope's workspace afterwards; `null` in repository scope.
    pub workspace: Option<Workspace>,
    /// Sentences for a person.
    pub notes: Vec<String>,
    /// The command that opens the pull request, shell-quoted; `null` when
    /// the host has none.
    pub next: Option<String>,
}

impl Published {
    /// From the response, the workspace read after it, and `quote`, which
    /// renders an argv as one shell line.
    pub fn from_response(
        response: &v1::PublishChangeResponse,
        workspace: Option<Workspace>,
        quote: impl Fn(&[String]) -> String,
    ) -> Self {
        let publication = response.publication.clone().unwrap_or_default();
        let next = publication
            .pull_request
            .as_ref()
            .filter(|hint| !hint.argv.is_empty())
            .map(|hint| quote(&hint.argv));
        Self {
            publication: Publication::from(&publication),
            operation: operation(response.operation.as_ref()),
            snapshot: operation(response.snapshot.as_ref()),
            describe: operation(response.describe.as_ref()),
            change: response.change.as_ref().map(Change::from),
            workspace,
            notes: response.notes.clone(),
            next,
        }
    }
}

/// `reef abandon`'s answer.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct FileEntry {
    /// Repository-relative, `/`-separated.
    pub path: String,
    /// `file`, `executable`, `symlink`, `directory`, `conflict`, or
    /// `submodule`.
    #[serde(rename = "type")]
    #[schemars(schema_with = "entry_type_name_schema_or_null")]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct FileRead {
    /// Repository-relative, `/`-separated.
    pub path: String,
    /// `file`, `executable`, `symlink`, or `conflict`.
    #[serde(rename = "type")]
    #[schemars(schema_with = "entry_type_name_schema_or_null")]
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ErrorDocument {
    /// The error.
    pub error: ErrorObject,
}

/// What `reef restore` prints (R1.18).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RestoredRepo {
    /// Its id, in hex.
    pub id: String,
    /// Its `.jj/repo`.
    pub jj_repo_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixed width at the edges RFC 3339 can write, `<millis>ms` past them.
    #[test]
    fn rfc3339_millis_edges() {
        assert_eq!(rfc3339_millis(0), "1970-01-01T00:00:00.000Z");
        assert_eq!(rfc3339_millis(-1000), "1969-12-31T23:59:59.000Z");
        assert_eq!(rfc3339_millis(-1), "1969-12-31T23:59:59.999Z");
        let year_zero = -62_167_219_200_000;
        assert_eq!(rfc3339_millis(year_zero), "0000-01-01T00:00:00.000Z");
        assert_eq!(
            rfc3339_millis(year_zero - 1),
            format!("{}ms", year_zero - 1)
        );
        assert_eq!(rfc3339_millis(i64::MIN), format!("{}ms", i64::MIN));
    }

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
        assert_eq!(value["lease"]["expires_at"], "2026-10-16T18:31:18.000Z");
        assert!(value["lease"].get("expires_at_ms").is_none());
        assert_eq!(value["lease"]["state"], "active");
        assert_eq!(value["lease"]["end_reason"], serde_json::Value::Null);
        assert_eq!(
            value["lease"]["heartbeat_every_ms"],
            serde_json::Value::Null
        );
        assert_eq!(value["stores"], serde_json::json!([]));
        assert_eq!(value["changed_file_count"], serde_json::Value::Null);
        // The last key: added after every other (ADR 0029).
        let text = serde_json::to_string(&Workspace::from(&ws)).unwrap();
        assert!(text.ends_with(r#","changed_file_count":null}"#), "{text}");
        let counted = v1::Workspace {
            changed_file_count: Some(3),
            ..ws
        };
        let value = serde_json::to_value(Workspace::from(&counted)).unwrap();
        assert_eq!(value["changed_file_count"], 3);
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
        let tables: [(Name, &[&str]); 8] = [
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
            (
                status_walk_reason_name,
                &[
                    "not-watched",
                    "watch-restarted",
                    "events-lost",
                    "too-broad",
                    "snapshot-failed",
                    "not-synced",
                    "stale-working-copy",
                    "path-not-narrowable",
                    "covered-by-full-scan",
                ],
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
                    "transition",
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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
    /// `activity`, `editing`, `lease`, `progress`, or `job`.
    #[schemars(schema_with = "presence_kind_schema")]
    pub kind: String,
    /// For `activity`.
    pub activity: Option<String>,
    /// For `editing`: repository-relative paths, most recent first.
    pub paths: Vec<String>,
    /// For `progress`.
    pub progress: Option<Progress>,
    /// For `lease`: the lease as of the heartbeat.
    pub lease: Option<Lease>,
    /// For `job` (R3.05): the job as of this report.
    pub job: Option<JobPresence>,
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
        Ok(v1::PresenceKind::Job) => "job",
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
            job: presence.job.as_ref().map(JobPresence::from),
            by_daemon: presence.by_daemon,
            updated_at: time(presence.updated_at.as_ref()),
            expires_at: time(presence.expires_at.as_ref()),
            updated_at_ms: presence.updated_at.as_ref().map_or(0, millis_of),
        }
    }
}

/// A change to one presence entry, as `reef presence watch` prints it
/// (R2.10).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
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

/// A symbol kind's name: `module`, `class`, `struct`, `interface`, `enum`,
/// `type`, `function`, `method`, `field`, `variable`, `constant` or
/// `macro`; `None` when unspecified (R2.14).
pub fn symbol_kind_name(kind: i32) -> Option<&'static str> {
    use v1::SymbolKind as K;
    let name = match K::try_from(kind) {
        Ok(K::Unspecified) => return None,
        Ok(K::Module) => "module",
        Ok(K::Class) => "class",
        Ok(K::Struct) => "struct",
        Ok(K::Interface) => "interface",
        Ok(K::Enum) => "enum",
        Ok(K::Type) => "type",
        Ok(K::Function) => "function",
        Ok(K::Method) => "method",
        Ok(K::Field) => "field",
        Ok(K::Variable) => "variable",
        Ok(K::Constant) => "constant",
        Ok(K::Macro) => "macro",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// The kind a name from [`symbol_kind_name`] stands for.
pub fn symbol_kind_of(name: &str) -> Option<v1::SymbolKind> {
    (1..=12)
        .filter_map(|n| v1::SymbolKind::try_from(n).ok())
        .find(|k| symbol_kind_name(*k as i32) == Some(name))
}

/// A symbol role's name: `definition` or `reference`.
pub fn symbol_role_name(role: i32) -> Option<&'static str> {
    let name = match v1::SymbolRole::try_from(role) {
        Ok(v1::SymbolRole::Unspecified) => return None,
        Ok(v1::SymbolRole::Definition) => "definition",
        Ok(v1::SymbolRole::Reference) => "reference",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// A symbol source's name: `tags` or `scip`.
pub fn symbol_source_name(source: i32) -> Option<&'static str> {
    let name = match v1::SymbolSource::try_from(source) {
        Ok(v1::SymbolSource::Unspecified) => return None,
        Ok(v1::SymbolSource::Tags) => "tags",
        Ok(v1::SymbolSource::Scip) => "scip",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// An index state's name: `not_indexed`, `indexing` or `complete`.
pub fn symbol_index_state_name(state: i32) -> Option<&'static str> {
    let name = match v1::SymbolIndexState::try_from(state) {
        Ok(v1::SymbolIndexState::Unspecified) => return None,
        Ok(v1::SymbolIndexState::NotIndexed) => "not_indexed",
        Ok(v1::SymbolIndexState::Indexing) => "indexing",
        Ok(v1::SymbolIndexState::Complete) => "complete",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// A skip reason's name: `too_large`, `binary`, `parse_timeout`,
/// `too_many_occurrences` or `conflicted`.
pub fn symbol_skip_reason_name(reason: i32) -> &'static str {
    use v1::SymbolSkipReason as R;
    match R::try_from(reason) {
        Ok(R::TooLarge) => "too_large",
        Ok(R::Binary) => "binary",
        Ok(R::ParseTimeout) => "parse_timeout",
        Ok(R::TooManyOccurrences) => "too_many_occurrences",
        Ok(R::Conflicted) => "conflicted",
        _ => UNKNOWN,
    }
}

/// An upload rejection's name, as the API's value without its prefix in
/// lower case: `invalid_path`, `content_mismatch` and so on.
pub fn scip_rejection_name(reason: i32) -> &'static str {
    use v1::ScipRejection as R;
    match R::try_from(reason) {
        Ok(R::InvalidPath) => "invalid_path",
        Ok(R::DuplicatePath) => "duplicate_path",
        Ok(R::PathNotFound) => "path_not_found",
        Ok(R::NotAFile) => "not_a_file",
        Ok(R::FileTooLarge) => "file_too_large",
        Ok(R::ContentMismatch) => "content_mismatch",
        Ok(R::InvalidDocument) => "invalid_document",
        Ok(R::TooManyOccurrences) => "too_many_occurrences",
        Ok(R::RangeOutOfBounds) => "range_out_of_bounds",
        _ => UNKNOWN,
    }
}

/// A range: `[start_line, start_column, end_line, end_column]`, 0-based
/// lines and UTF-8 byte columns, end exclusive.
pub type SourceRange = [u32; 4];

fn source_range(range: Option<&v1::SourceRange>) -> Option<SourceRange> {
    range.map(|r| [r.start_line, r.start_column, r.end_line, r.end_column])
}

/// A tree's indexing, as `reef symbols index` prints it (R2.14).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SymbolStatus {
    /// The commit.
    pub commit_id: String,
    /// `not_indexed`, `indexing` or `complete`.
    #[schemars(schema_with = "symbol_index_state_name_schema_or_null")]
    pub state: Option<String>,
    /// Supported files.
    pub files: u64,
    /// Files with an entry, skipped ones included.
    pub files_indexed: u64,
    /// Files skipped.
    pub files_skipped: u64,
    /// Skipped files by reason.
    pub skipped: BTreeMap<String, u64>,
    /// Files with an uploaded SCIP document.
    pub scip_files: u64,
    /// Supported files by language.
    pub languages: BTreeMap<String, u64>,
    /// When the last pass ended.
    pub indexed_at: Option<String>,
}

impl From<&v1::SymbolIndexStatus> for SymbolStatus {
    fn from(status: &v1::SymbolIndexStatus) -> Self {
        Self {
            commit_id: status.commit_id.clone(),
            state: name(symbol_index_state_name(status.state)),
            files: status.files,
            files_indexed: status.files_indexed,
            files_skipped: status.files_skipped,
            skipped: status
                .skipped
                .iter()
                .map(|s| (symbol_skip_reason_name(s.reason).to_owned(), s.files))
                .collect(),
            scip_files: status.scip_files,
            languages: status
                .languages
                .iter()
                .map(|l| (l.language.clone(), l.files))
                .collect(),
            indexed_at: time(status.indexed_at.as_ref()),
        }
    }
}

/// One occurrence, as `reef symbols list` prints it (R2.14).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SymbolOccurrence {
    /// The file.
    pub path: String,
    /// The display name.
    pub name: String,
    /// The SCIP symbol string; `null` for a tags reference.
    pub symbol: Option<String>,
    /// What it is.
    #[schemars(schema_with = "symbol_kind_name_schema_or_null")]
    pub kind: Option<String>,
    /// `definition` or `reference`.
    #[schemars(schema_with = "symbol_role_name_schema_or_null")]
    pub role: Option<String>,
    /// The name's range.
    pub range: Option<SourceRange>,
    /// A definition's whole range.
    pub definition_range: Option<SourceRange>,
    /// The innermost definition containing it.
    pub enclosing_symbol: Option<String>,
    /// `tags` or `scip`.
    #[schemars(schema_with = "symbol_source_name_schema_or_null")]
    pub source: Option<String>,
    /// The file's language.
    pub language: Option<String>,
    /// SCIP's roles beyond definition: `import`, `write`, `test`,
    /// `generated`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<&'static str>,
}

impl From<&v1::SymbolOccurrence> for SymbolOccurrence {
    fn from(o: &v1::SymbolOccurrence) -> Self {
        let flags = [
            (o.is_import, "import"),
            (o.is_write, "write"),
            (o.is_test, "test"),
            (o.is_generated, "generated"),
        ]
        .into_iter()
        .filter_map(|(set, flag)| set.then_some(flag))
        .collect();
        Self {
            path: o.path.clone(),
            name: o.name.clone(),
            symbol: text(&o.symbol),
            kind: name(symbol_kind_name(o.kind)),
            role: name(symbol_role_name(o.role)),
            range: source_range(o.range.as_ref()),
            definition_range: source_range(o.definition_range.as_ref()),
            enclosing_symbol: text(&o.enclosing_symbol),
            source: name(symbol_source_name(o.source)),
            language: text(&o.language),
            flags,
        }
    }
}

/// A document an upload did not store, or did not send.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct UploadProblem {
    /// Its repository path.
    pub path: String,
    /// Why: a rejection's name, or `unreadable` and `too_large` for one the
    /// CLI did not send.
    #[schemars(schema_with = "upload_problem_reason_schema")]
    pub reason: String,
    /// In words; absent for one not sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What `reef symbols upload` did (R2.14).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SymbolUpload {
    /// The commit.
    pub commit_id: String,
    /// Documents stored.
    pub accepted: u64,
    /// Documents the daemon rejected.
    pub rejected: Vec<UploadProblem>,
    /// Documents the CLI did not send.
    pub skipped: Vec<UploadProblem>,
    /// The tree's status afterwards.
    pub status: Option<SymbolStatus>,
}

/// What became of a change in the history index (R2.16): `open`,
/// `landed`, `abandoned` or `undone`; `None` when unset.
pub fn change_outcome_name(outcome: i32) -> Option<&'static str> {
    use v1::ChangeOutcome as O;
    let name = match O::try_from(outcome) {
        Ok(O::Unspecified) => return None,
        Ok(O::Open) => "open",
        Ok(O::Landed) => "landed",
        Ok(O::Abandoned) => "abandoned",
        Ok(O::Undone) => "undone",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

fn touch_state_name(state: i32) -> Option<&'static str> {
    use v1::TouchState as S;
    let name = match S::try_from(state) {
        Ok(S::Unspecified) => return None,
        Ok(S::Pending) => "pending",
        Ok(S::Current) => "current",
        Ok(S::Stale) => "stale",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

fn history_symbol_coverage_name(coverage: i32) -> Option<&'static str> {
    use v1::HistorySymbolCoverage as C;
    let name = match C::try_from(coverage) {
        Ok(C::Unspecified) => return None,
        Ok(C::Covered) => "covered",
        Ok(C::Partial) => "partial",
        Ok(C::TooLarge) => "too_large",
        Ok(C::Disabled) => "disabled",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// One agent's writes to a change in one workspace (R2.16).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ChangeActivity {
    /// The agent; `null` for another jj client's writes and imports.
    pub agent: Option<String>,
    /// The workspace; `null` for none.
    pub workspace_id: Option<String>,
    /// The first write.
    pub first_at: Option<String>,
    /// The last write.
    pub last_at: Option<String>,
    /// How many operations wrote it.
    pub operations: u32,
    /// The last of them; `null` once GC abandoned it.
    pub last_operation_id: Option<String>,
}

impl From<&v1::ChangeActivity> for ChangeActivity {
    fn from(a: &v1::ChangeActivity) -> Self {
        Self {
            agent: text(&a.agent),
            workspace_id: text(&a.workspace_id),
            first_at: time(a.first_at.as_ref()),
            last_at: time(a.last_at.as_ref()),
            operations: a.operations,
            last_operation_id: text(&a.last_operation_id),
        }
    }
}

/// A definition a change's net diff touched (R2.16).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SymbolTouch {
    /// Its symbol.
    pub symbol: String,
    /// Its old symbol when moved under another; else `null`.
    pub old_symbol: Option<String>,
    /// Its display name.
    pub name: String,
    /// What it is.
    #[schemars(schema_with = "symbol_kind_name_schema_or_null")]
    pub kind: Option<String>,
    /// How it changed.
    #[schemars(schema_with = "symbol_change_kind_name_schema_or_null")]
    pub change: Option<String>,
    /// Its file.
    pub path: String,
}

impl From<&v1::SymbolTouch> for SymbolTouch {
    fn from(t: &v1::SymbolTouch) -> Self {
        Self {
            symbol: t.symbol.clone(),
            old_symbol: text(&t.old_symbol),
            name: t.name.clone(),
            kind: name(symbol_kind_name(t.kind)),
            change: name(symbol_change_kind_name(t.change)),
            path: t.path.clone(),
        }
    }
}

/// One change of the history index (R2.16).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct HistoryChange {
    /// The change id.
    pub change_id: String,
    /// The same in jj's reverse hex.
    pub change_id_reverse_hex: String,
    /// Its latest commit.
    pub commit_id: String,
    /// What became of it.
    #[schemars(schema_with = "change_outcome_name_schema_or_null")]
    pub outcome: Option<String>,
    /// More than one visible commit.
    pub divergent: bool,
    /// Its net diff is empty.
    pub empty: bool,
    /// Its description's first line.
    pub subject: String,
    /// Its author's email.
    pub author_email: Option<String>,
    /// The first write.
    pub first_at: Option<String>,
    /// The last write.
    pub last_at: Option<String>,
    /// When it was seen landing.
    pub landed_at: Option<String>,
    /// When it was abandoned or undone.
    pub ended_at: Option<String>,
    /// Who wrote it, the latest first.
    pub activity: Vec<ChangeActivity>,
    /// Activity rows in all.
    pub activity_count: u32,
    /// The matched symbol touches.
    pub touches: Vec<SymbolTouch>,
    /// Matched touches in all.
    pub touch_count: u32,
    /// The matched paths.
    pub matched_paths: Vec<String>,
    /// Paths its net diff touches.
    pub paths_changed: u32,
    /// Only the first 10,000 paths are indexed.
    pub paths_truncated: bool,
    /// Symbols its net diff touches.
    pub symbols_changed: u32,
    /// `pending`, `current` or `stale`.
    #[schemars(schema_with = "touch_state_name_schema_or_null")]
    pub touches_state: Option<String>,
    /// `covered`, `partial`, `too_large` or `disabled`.
    #[schemars(schema_with = "history_symbol_coverage_name_schema_or_null")]
    pub symbols_coverage: Option<String>,
}

impl From<&v1::HistoryChange> for HistoryChange {
    fn from(c: &v1::HistoryChange) -> Self {
        Self {
            change_id: c.change_id.clone(),
            change_id_reverse_hex: reverse_hex(&c.change_id),
            commit_id: c.commit_id.clone(),
            outcome: name(change_outcome_name(c.outcome)),
            divergent: c.divergent,
            empty: c.empty,
            subject: c.subject.clone(),
            author_email: text(&c.author_email),
            first_at: time(c.first_at.as_ref()),
            last_at: time(c.last_at.as_ref()),
            landed_at: time(c.landed_at.as_ref()),
            ended_at: time(c.ended_at.as_ref()),
            activity: c.activity.iter().map(ChangeActivity::from).collect(),
            activity_count: c.activity_count,
            touches: c.touches.iter().map(SymbolTouch::from).collect(),
            touch_count: c.touch_count,
            matched_paths: c.matched_paths.clone(),
            paths_changed: c.paths_changed,
            paths_truncated: c.paths_truncated,
            symbols_changed: c.symbols_changed,
            touches_state: name(touch_state_name(c.touches_state)),
            symbols_coverage: name(history_symbol_coverage_name(c.symbols_coverage)),
        }
    }
}

/// One group's count (R2.16).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ChangeGroup {
    /// The agent, when grouped by agent.
    pub agent: Option<String>,
    /// The workspace, when grouped by workspace.
    pub workspace_id: Option<String>,
    /// The outcome, when grouped by outcome.
    #[schemars(schema_with = "change_outcome_name_schema_or_null")]
    pub outcome: Option<String>,
    /// How many.
    pub count: u32,
}

impl From<&v1::ChangeGroup> for ChangeGroup {
    fn from(g: &v1::ChangeGroup) -> Self {
        Self {
            agent: text(&g.agent),
            workspace_id: text(&g.workspace_id),
            outcome: name(change_outcome_name(g.outcome)),
            count: g.count,
        }
    }
}

/// How much history an answer covers (R2.16).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct HistoryCoverage {
    /// The earliest write the index holds.
    pub oldest_at: Option<String>,
    /// Older operations are still being read.
    pub backfilling: bool,
    /// Operations not read yet, up to 10,000.
    pub behind: u32,
    /// Changes whose touches are not computed yet.
    pub pending: u32,
    /// Symbol touches are computed.
    pub symbols: bool,
}

impl From<&v1::HistoryCoverage> for HistoryCoverage {
    fn from(c: &v1::HistoryCoverage) -> Self {
        Self {
            oldest_at: time(c.oldest_at.as_ref()),
            backfilling: c.backfilling,
            behind: c.behind,
            pending: c.pending,
            symbols: c.symbols,
        }
    }
}

/// `reef history`'s answer (R2.16): changes or groups, and the coverage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct History {
    /// The changes, newest activity first; empty with `--group-by`.
    pub changes: Vec<HistoryChange>,
    /// The counts; empty without `--group-by`.
    pub groups: Vec<ChangeGroup>,
    /// More groups matched than listed.
    pub groups_truncated: bool,
    /// How much history the answer covers.
    pub coverage: HistoryCoverage,
}

impl From<&v1::QueryChangesResponse> for History {
    fn from(r: &v1::QueryChangesResponse) -> Self {
        Self {
            changes: r.changes.iter().map(HistoryChange::from).collect(),
            groups: r.groups.iter().map(ChangeGroup::from).collect(),
            groups_truncated: r.groups_truncated,
            coverage: r
                .coverage
                .as_ref()
                .map(HistoryCoverage::from)
                .unwrap_or_default(),
        }
    }
}

/// An evidence result's name: `pass`, `fail` or `error` (R3.01); `None`
/// when unspecified.
pub fn evidence_result_name(result: i32) -> Option<&'static str> {
    let name = match v1::EvidenceResult::try_from(result) {
        Ok(v1::EvidenceResult::Unspecified) => return None,
        Ok(v1::EvidenceResult::Pass) => "pass",
        Ok(v1::EvidenceResult::Fail) => "fail",
        Ok(v1::EvidenceResult::Error) => "error",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// The result a name from [`evidence_result_name`] stands for.
pub fn evidence_result_of(name: &str) -> Option<v1::EvidenceResult> {
    (1..=3)
        .filter_map(|n| v1::EvidenceResult::try_from(n).ok())
        .find(|r| evidence_result_name(*r as i32) == Some(name))
}

/// A signature check's name: `valid` or `invalid` (R3.01); `None` when
/// unspecified.
pub fn signature_check_name(check: i32) -> Option<&'static str> {
    let name = match v1::SignatureCheck::try_from(check) {
        Ok(v1::SignatureCheck::Unspecified) => return None,
        Ok(v1::SignatureCheck::Valid) => "valid",
        Ok(v1::SignatureCheck::Invalid) => "invalid",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// An artifact state's name: `stored`, `collected` or `absent` (R3.02);
/// `None` when unspecified.
pub fn artifact_state_name(state: i32) -> Option<&'static str> {
    let name = match v1::ArtifactState::try_from(state) {
        Ok(v1::ArtifactState::Unspecified) => return None,
        Ok(v1::ArtifactState::Stored) => "stored",
        Ok(v1::ArtifactState::Collected) => "collected",
        Ok(v1::ArtifactState::Absent) => "absent",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

names_schema! {
    artifact_state_name_schema_or_null = artifact_state_name, true;
    evidence_result_name_schema_or_null = evidence_result_name, true;
    signature_check_name_schema_or_null = signature_check_name, true;
}

/// An artifact an evidence object records, by digest (R3.01), with what the
/// artifact store holds of it (R3.02).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct EvidenceArtifact {
    /// Its name, such as `test.log`.
    pub name: String,
    /// SHA-256 of its bytes, lowercase hex.
    pub sha256: String,
    /// Where it can be fetched.
    pub uri: Option<String>,
    /// Its media type.
    pub media_type: Option<String>,
    /// `stored`, `collected` (retention removed the bytes), or `absent`
    /// (never uploaded, or deleted when no evidence named it).
    #[schemars(schema_with = "artifact_state_name_schema_or_null")]
    pub state: Option<String>,
    /// Its length in bytes; 0 when absent.
    pub size: u64,
    /// When retention removed its bytes.
    pub collected_at: Option<String>,
}

/// An artifact's record (R3.02): `reef evidence artifact show`, and `read`
/// with `-o json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Artifact {
    /// SHA-256 of its bytes, lowercase hex.
    pub sha256: String,
    /// `stored`, `collected` (retention removed the bytes), or `absent`
    /// (named by evidence, never stored or deleted when nothing named it).
    #[schemars(schema_with = "artifact_state_name_schema_or_null")]
    pub state: Option<String>,
    /// Its length in bytes; 0 when absent.
    pub size: u64,
    /// When its bytes were last stored.
    pub stored_at: Option<String>,
    /// When it was last stored, uploaded again, or named by evidence added.
    pub used_at: Option<String>,
    /// When retention removed its bytes.
    pub collected_at: Option<String>,
    /// The `reef-agent` of the upload that last stored its bytes.
    pub added_by_agent: Option<String>,
    /// That upload's remote token (R2.08).
    pub remote_token_id: Option<String>,
    /// How many evidence objects name it.
    pub evidence_count: u32,
    /// The newest 20 of them, newest added first.
    pub evidence_ids: Vec<String>,
}

impl From<&v1::Artifact> for Artifact {
    fn from(artifact: &v1::Artifact) -> Self {
        Self {
            sha256: artifact.sha256.clone(),
            state: name(artifact_state_name(artifact.state)),
            size: artifact.size,
            stored_at: time(artifact.stored_at.as_ref()),
            used_at: time(artifact.used_at.as_ref()),
            collected_at: time(artifact.collected_at.as_ref()),
            added_by_agent: text(&artifact.added_by_agent),
            remote_token_id: text(&artifact.remote_token_id),
            evidence_count: artifact.evidence_count,
            evidence_ids: artifact.evidence_ids.clone(),
        }
    }
}

/// One file `reef evidence artifact upload` sent (R3.02).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ArtifactUploaded {
    /// The artifact as stored.
    pub artifact: Artifact,
    /// Whether this upload stored it; false when it was stored already.
    pub created: bool,
    /// The file, as given.
    pub path: String,
}

/// An evidence object, as `reef evidence list` and `show` print it (R3.01,
/// ADR 0055): a signed claim about a tree. `signature` says whether its
/// signature verified when it was read, never whether its key is trusted.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct Evidence {
    /// The SHA-256 of the canonical envelope, lowercase hex.
    pub id: String,
    /// The tree the claim is about.
    pub tree_id: String,
    /// What was checked: `test`, `build`, `lint`, `policy`, or another kind.
    pub kind: String,
    /// `pass`, `fail`, or `error` (no verdict).
    #[schemars(schema_with = "evidence_result_name_schema_or_null")]
    pub result: Option<String>,
    /// The target covered; `null` for the whole check.
    pub target: Option<String>,
    /// The id of the key that signed it: SHA-256 of its DER
    /// `SubjectPublicKeyInfo`. The attestor is this key.
    pub key_id: String,
    /// When the check started, as the attestor claims.
    pub started_at: Option<String>,
    /// When it finished, as the attestor claims.
    pub finished_at: Option<String>,
    /// How long it ran, in milliseconds, by the claim's times.
    pub duration_ms: Option<u64>,
    /// When it was first added in this repository.
    pub added_at: Option<String>,
    /// The `reef-agent` that first added it: who uploaded it, not who
    /// attested it.
    pub added_by_agent: Option<String>,
    /// The remote token that first added it (R2.08).
    pub remote_token_id: Option<String>,
    /// What the check produced.
    pub artifacts: Vec<EvidenceArtifact>,
    /// The changes it was added for, in hex.
    pub change_ids: Vec<String>,
    /// `valid` when its id, signature and key id check out at this read;
    /// `invalid` when the stored object was changed.
    #[schemars(schema_with = "signature_check_name_schema_or_null")]
    pub signature: Option<String>,
    /// The payload's predicate type.
    pub predicate_type: String,
    /// In a listing by change: whether it is on the change's tree now.
    pub current: bool,
    /// The payload, an in-toto Statement, as JSON (`show` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<serde_json::Value>,
    /// The canonical DSSE envelope, as JSON (`show` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope: Option<serde_json::Value>,
    /// Which operator identity the signing key is bound to, and its status
    /// when the daemon first witnessed the object (R3.03). Beside
    /// `signature`, never a trust decision: policy decides whom to trust.
    pub identity: Option<KeyIdentity>,
}

impl From<&v1::Evidence> for Evidence {
    fn from(evidence: &v1::Evidence) -> Self {
        let envelope: Option<serde_json::Value> = (!evidence.envelope.is_empty())
            .then(|| serde_json::from_slice(&evidence.envelope).ok())
            .flatten();
        let statement = envelope
            .as_ref()
            .and_then(|value| value.get("payload"))
            .and_then(serde_json::Value::as_str)
            .and_then(|payload| STANDARD.decode(payload).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        let millis = |at: Option<&prost_types::Timestamp>| at.map(millis_of);
        let duration_ms = match (
            millis(evidence.started_at.as_ref()),
            millis(evidence.finished_at.as_ref()),
        ) {
            (Some(start), Some(end)) => u64::try_from(end - start).ok(),
            _ => None,
        };
        Self {
            id: evidence.evidence_id.clone(),
            tree_id: evidence.tree_id.clone(),
            kind: evidence.kind.clone(),
            result: name(evidence_result_name(evidence.result)),
            target: text(&evidence.target),
            key_id: evidence.key_id.clone(),
            started_at: time(evidence.started_at.as_ref()),
            finished_at: time(evidence.finished_at.as_ref()),
            duration_ms,
            added_at: time(evidence.added_at.as_ref()),
            added_by_agent: text(&evidence.added_by_agent),
            remote_token_id: text(&evidence.remote_token_id),
            artifacts: evidence
                .artifacts
                .iter()
                .map(|artifact| EvidenceArtifact {
                    name: artifact.name.clone(),
                    sha256: artifact.sha256.clone(),
                    uri: text(&artifact.uri),
                    media_type: text(&artifact.media_type),
                    state: name(artifact_state_name(artifact.state)),
                    size: artifact.size,
                    collected_at: time(artifact.collected_at.as_ref()),
                })
                .collect(),
            change_ids: evidence.change_ids.clone(),
            signature: name(signature_check_name(evidence.signature)),
            predicate_type: evidence.predicate_type.clone(),
            current: evidence.current,
            statement,
            envelope,
            identity: evidence.identity.as_ref().map(KeyIdentity::from),
        }
    }
}

/// What `reef evidence add` stored (R3.01).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct EvidenceAdded {
    /// The object as stored.
    pub evidence: Evidence,
    /// Whether this add stored it; false when it was already stored.
    pub created: bool,
}

/// A key `reef evidence key generate` wrote (R3.01).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct EvidenceKey {
    /// SHA-256 of the public key's DER `SubjectPublicKeyInfo`, lowercase
    /// hex: the id evidence it signs carries.
    pub key_id: String,
    /// The private key, PKCS#8 PEM, mode 0600.
    pub path: String,
    /// The public key, SPKI PEM: `<path>.pub`.
    pub public_key_path: String,
}

/// An identity kind's name: `person` or `ci` (R3.03); `None` when
/// unspecified.
pub fn identity_kind_name(kind: i32) -> Option<&'static str> {
    let name = match v1::IdentityKind::try_from(kind) {
        Ok(v1::IdentityKind::Unspecified) => return None,
        Ok(v1::IdentityKind::Person) => "person",
        Ok(v1::IdentityKind::Ci) => "ci",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// A session actor's name: `human`, `agent` or `executor` (R3.03); `None`
/// when unspecified.
pub fn session_actor_name(actor: i32) -> Option<&'static str> {
    let name = match v1::SessionActor::try_from(actor) {
        Ok(v1::SessionActor::Unspecified) => return None,
        Ok(v1::SessionActor::Human) => "human",
        Ok(v1::SessionActor::Agent) => "agent",
        Ok(v1::SessionActor::Executor) => "executor",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// The actor a name from [`session_actor_name`] stands for.
pub fn session_actor_of(name: &str) -> Option<v1::SessionActor> {
    (1..=3)
        .filter_map(|n| v1::SessionActor::try_from(n).ok())
        .find(|actor| session_actor_name(*actor as i32) == Some(name))
}

/// A presence's name: `none` (R3.03); `None` when unspecified.
pub fn session_presence_name(presence: i32) -> Option<&'static str> {
    let name = match v1::SessionPresence::try_from(presence) {
        Ok(v1::SessionPresence::Unspecified) => return None,
        Ok(v1::SessionPresence::None) => "none",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// A key custody's name: `client` or `daemon` (R3.03); `None` when
/// unspecified.
pub fn key_custody_name(custody: i32) -> Option<&'static str> {
    let name = match v1::KeyCustody::try_from(custody) {
        Ok(v1::KeyCustody::Unspecified) => return None,
        Ok(v1::KeyCustody::Client) => "client",
        Ok(v1::KeyCustody::Daemon) => "daemon",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// An issuer kind's name: `reef-daemon` (R3.03) or `sigstore` (R3.42);
/// `None` when unspecified.
pub fn issuer_kind_name(kind: i32) -> Option<&'static str> {
    let name = match v1::IssuerKind::try_from(kind) {
        Ok(v1::IssuerKind::Unspecified) => return None,
        Ok(v1::IssuerKind::ReefDaemon) => "reef-daemon",
        Ok(v1::IssuerKind::Sigstore) => "sigstore",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// An identity status's name: `unbound`, `bound`, `expired`, `revoked` or
/// `invalid` (R3.03); `None` when unspecified.
pub fn identity_status_name(status: i32) -> Option<&'static str> {
    let name = match v1::IdentityStatus::try_from(status) {
        Ok(v1::IdentityStatus::Unspecified) => return None,
        Ok(v1::IdentityStatus::Unbound) => "unbound",
        Ok(v1::IdentityStatus::Bound) => "bound",
        Ok(v1::IdentityStatus::Expired) => "expired",
        Ok(v1::IdentityStatus::Revoked) => "revoked",
        Ok(v1::IdentityStatus::Invalid) => "invalid",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

names_schema! {
    identity_kind_name_schema_or_null = identity_kind_name, true;
    session_actor_name_schema_or_null = session_actor_name, true;
    session_presence_name_schema_or_null = session_presence_name, true;
    key_custody_name_schema_or_null = key_custody_name, true;
    issuer_kind_name_schema_or_null = issuer_kind_name, true;
    identity_status_name_schema_or_null = identity_status_name, true;
}

/// A revocation (R3.03): when it was made, and from when signatures stop
/// counting.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Revocation {
    /// When it was made.
    pub revoked_at: Option<String>,
    /// From when signatures stop counting; never after `revoked_at`.
    pub invalid_from: Option<String>,
    /// Why, as the revoker said.
    pub reason: Option<String>,
}

impl From<&v1::Revocation> for Revocation {
    fn from(revocation: &v1::Revocation) -> Self {
        Self {
            revoked_at: time(revocation.revoked_at.as_ref()),
            invalid_from: time(revocation.invalid_from.as_ref()),
            reason: text(&revocation.reason),
        }
    }
}

/// An operator identity (R3.03): the person or CI pipeline a key answers
/// to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Identity {
    /// `idn_` and 16 hex digits, never reused.
    pub id: String,
    /// Its name, never reused.
    pub name: String,
    /// `person` or `ci`.
    #[schemars(schema_with = "identity_kind_name_schema_or_null")]
    pub kind: Option<String>,
    /// For display, unverified.
    pub display_name: Option<String>,
    /// For display, checked for its form only.
    pub email: Option<String>,
    /// When it was created.
    pub created_at: Option<String>,
    /// Its revocation; `null` while live.
    pub revocation: Option<Revocation>,
    /// Whether socket requests that name no identity use it.
    pub is_default: bool,
    /// Its session keys neither expired nor revoked.
    pub live_session_keys: u32,
    /// The live remote tokens linked to it.
    pub remote_token_names: Vec<String>,
    /// The OIDC issuers and subjects whose keyless certificates it accepts
    /// (R3.42), oldest first, revoked ones included.
    pub oidc_matches: Vec<OidcMatch>,
}

/// An exact OIDC issuer and subject an identity accepts keyless
/// certificates for (R3.42).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct OidcMatch {
    /// `oidc_` and 16 hex digits.
    pub id: String,
    /// The identity's id.
    pub identity_id: String,
    /// The OIDC issuer, exact.
    pub issuer: String,
    /// The certificate's subject (its SAN), exact.
    pub subject: String,
    /// When it was added.
    pub created_at: Option<String>,
    /// Its revocation, which applies to every key bound through it; `null`
    /// while live.
    pub revocation: Option<Revocation>,
}

impl From<&v1::OidcMatch> for OidcMatch {
    fn from(row: &v1::OidcMatch) -> Self {
        Self {
            id: row.id.clone(),
            identity_id: row.identity_id.clone(),
            issuer: row.issuer.clone(),
            subject: row.subject.clone(),
            created_at: time(row.created_at.as_ref()),
            revocation: row.revocation.as_ref().map(Revocation::from),
        }
    }
}

/// What `reef identity oidc revoke` covered (R3.42).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct OidcMatchRevoked {
    /// The match, with its revocation.
    #[serde(rename = "match")]
    pub matched: OidcMatch,
    /// The keys bound through it, newest first, up to 1,000.
    pub key_ids: Vec<String>,
}

/// What a keyless key's Sigstore certificate says (R3.42).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SigstoreCertificate {
    /// The certificate's OIDC issuer.
    pub oidc_issuer: String,
    /// Its subject: an email or a URI.
    pub oidc_subject: String,
    /// The OIDC match it was bound through.
    pub oidc_match_id: String,
    /// When the log witnessed the binding statement.
    pub integrated_time: Option<String>,
    /// The log's id.
    pub log_id: String,
    /// The entry's index in the log.
    pub log_index: u64,
    /// Fulcio's CI claims, by name.
    pub ci_claims: BTreeMap<String, String>,
    /// SHA-256 of the registered bundle.
    pub bundle_id: String,
}

impl From<&v1::SigstoreCertificate> for SigstoreCertificate {
    fn from(certificate: &v1::SigstoreCertificate) -> Self {
        Self {
            oidc_issuer: certificate.oidc_issuer.clone(),
            oidc_subject: certificate.oidc_subject.clone(),
            oidc_match_id: certificate.oidc_match_id.clone(),
            integrated_time: time(certificate.integrated_time.as_ref()),
            log_id: certificate.log_id.clone(),
            log_index: certificate.log_index,
            ci_claims: certificate
                .ci_claims
                .iter()
                .map(|(name, value)| (name.clone(), value.clone()))
                .collect(),
            bundle_id: certificate.bundle_id.clone(),
        }
    }
}

impl From<&v1::Identity> for Identity {
    fn from(identity: &v1::Identity) -> Self {
        Self {
            id: identity.id.clone(),
            name: identity.name.clone(),
            kind: name(identity_kind_name(identity.kind)),
            display_name: text(&identity.display_name),
            email: text(&identity.email),
            created_at: time(identity.created_at.as_ref()),
            revocation: identity.revocation.as_ref().map(Revocation::from),
            is_default: identity.is_default,
            live_session_keys: identity.live_session_keys,
            remote_token_names: identity.remote_token_names.clone(),
            oidc_matches: identity.oidc_matches.iter().map(OidcMatch::from).collect(),
        }
    }
}

/// A session key and what its certificate binds it to (R3.03).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SessionKey {
    /// The key's id: the `key_id` of evidence it signs.
    pub key_id: String,
    /// The identity it answers to; empty, as every certificate field is,
    /// when `certificate_valid` is false.
    pub identity: String,
    /// That identity's id.
    pub identity_id: String,
    /// That identity's kind: `person` or `ci`.
    #[schemars(schema_with = "identity_kind_name_schema_or_null")]
    pub identity_kind: Option<String>,
    /// What the requester declared it for: `human`, `agent` or `executor`.
    #[schemars(schema_with = "session_actor_name_schema_or_null")]
    pub actor: Option<String>,
    /// The issuing request's agent label: declared, unverified.
    pub agent: Option<String>,
    /// Its session; successive keys of one session share it.
    pub session_id: String,
    /// How a human's presence was established: `none` (not verified).
    #[schemars(schema_with = "session_presence_name_schema_or_null")]
    pub presence: Option<String>,
    /// Where its private half lives: `client` or `daemon`.
    #[schemars(schema_with = "key_custody_name_schema_or_null")]
    pub custody: Option<String>,
    /// Who issued its certificate: `reef-daemon`, or `sigstore` for a
    /// keyless key (R3.42).
    #[schemars(schema_with = "issuer_kind_name_schema_or_null")]
    pub issuer_kind: Option<String>,
    /// The issuing authority's key id.
    pub authority_key_id: String,
    /// The start of its validity window.
    pub not_before: Option<String>,
    /// Its end, inclusive.
    pub not_after: Option<String>,
    /// When the daemon issued it.
    pub issued_at: Option<String>,
    /// The remote token of the issuing request (R2.08).
    pub remote_token_id: Option<String>,
    /// The earliest applicable revocation: the key's, its session's, its
    /// identity's or its authority's.
    pub revocation: Option<Revocation>,
    /// Whether its certificate verified at this read.
    pub certificate_valid: bool,
    /// A keyless key's Sigstore certificate (R3.42); `null` otherwise.
    pub sigstore: Option<SigstoreCertificate>,
}

impl From<&v1::SessionKey> for SessionKey {
    fn from(key: &v1::SessionKey) -> Self {
        Self {
            key_id: key.key_id.clone(),
            identity: key.identity.clone(),
            identity_id: key.identity_id.clone(),
            identity_kind: name(identity_kind_name(key.identity_kind)),
            actor: name(session_actor_name(key.actor)),
            agent: text(&key.agent),
            session_id: key.session_id.clone(),
            presence: name(session_presence_name(key.presence)),
            custody: name(key_custody_name(key.custody)),
            issuer_kind: name(issuer_kind_name(key.issuer_kind)),
            authority_key_id: key.authority_key_id.clone(),
            not_before: time(key.not_before.as_ref()),
            not_after: time(key.not_after.as_ref()),
            issued_at: time(key.issued_at.as_ref()),
            remote_token_id: text(&key.remote_token_id),
            revocation: key.revocation.as_ref().map(Revocation::from),
            certificate_valid: key.certificate_valid,
            sigstore: key.sigstore.as_ref().map(SigstoreCertificate::from),
        }
    }
}

/// A session key `reef identity session issue` made (R3.03): its record,
/// its private key, and its certificate. A credential: the private key is
/// printed once, here, and Reef keeps it nowhere.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct SessionKeyIssued {
    /// The key's record.
    pub key: SessionKey,
    /// The private key, PKCS#8 PEM (`-----BEGIN PRIVATE KEY-----`).
    pub private_key: String,
    /// The certificate: the DSSE envelope the daemon's authority signed, as
    /// JSON.
    pub certificate: serde_json::Value,
}

impl Drop for SessionKeyIssued {
    /// The private key's text is wiped as the value goes; copies a caller
    /// made of it, such as a serialized buffer, are the caller's to wipe.
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.private_key);
    }
}

/// One of the daemon's issuing authority keys (R3.03).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Authority {
    /// Its key id: the certificates' `keyid`.
    pub key_id: String,
    /// `ed25519`.
    pub algorithm: String,
    /// Its public key, SPKI PEM: what another party pins.
    pub public_key: String,
    /// The host it was made on.
    pub host: String,
    /// When it was made.
    pub created_at: Option<String>,
    /// When it stopped issuing; `null` for the current one.
    pub retired_at: Option<String>,
    /// Its revocation, which applies to everything it issued.
    pub revocation: Option<Revocation>,
    /// Whether it is the one that issues now.
    pub current: bool,
    /// `reef-daemon`, or `sigstore` for a Fulcio CA a keyless key came
    /// from (R3.42).
    #[schemars(schema_with = "issuer_kind_name_schema_or_null")]
    pub kind: Option<String>,
}

impl From<&v1::Authority> for Authority {
    fn from(authority: &v1::Authority) -> Self {
        let public_key = reef_evidence::keys::pem_encode("PUBLIC KEY", &authority.public_key);
        Self {
            key_id: authority.key_id.clone(),
            algorithm: authority.algorithm.clone(),
            public_key,
            host: authority.host.clone(),
            created_at: time(authority.created_at.as_ref()),
            retired_at: time(authority.retired_at.as_ref()),
            revocation: authority.revocation.as_ref().map(Revocation::from),
            current: authority.current,
            kind: name(issuer_kind_name(authority.kind)),
        }
    }
}

/// What a verifier sees of the key that signed an evidence object
/// (R3.03).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct KeyIdentity {
    /// `unbound` (a bare key), `bound`, `expired`, `revoked`, or `invalid`
    /// (the certificate does not verify), at `witnessed_at`.
    #[schemars(schema_with = "identity_status_name_schema_or_null")]
    pub status: Option<String>,
    /// The certified key; `null` when unbound.
    pub key: Option<SessionKey>,
    /// When the daemon first witnessed the object: its `added_at`.
    pub witnessed_at: Option<String>,
}

impl From<&v1::KeyIdentity> for KeyIdentity {
    fn from(identity: &v1::KeyIdentity) -> Self {
        Self {
            status: name(identity_status_name(identity.status)),
            key: identity.key.as_ref().map(SessionKey::from),
            witnessed_at: time(identity.witnessed_at.as_ref()),
        }
    }
}

/// What `reef identity authority rotate` did (R3.03).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct AuthorityRotated {
    /// The key that stopped issuing, revoked when asked.
    pub retired: Authority,
    /// The key that issues now.
    pub current: Authority,
}

/// A declared provenance context (R3.04): every field a client's claim,
/// which Reef records and signs but never verifies.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProvenanceContext {
    /// The model and its version.
    pub model: Option<String>,
    /// The agent runtime and its version.
    pub harness: Option<String>,
    /// The originating task.
    pub task: Option<String>,
    /// The SHA-256 of the instruction's exact bytes; the text is never
    /// sent.
    pub instruction_sha256: Option<String>,
    /// The harness's own session id.
    pub client_session: Option<String>,
    /// When it was last declared.
    pub declared_at: Option<String>,
}

impl From<&v1::ProvenanceContext> for ProvenanceContext {
    fn from(context: &v1::ProvenanceContext) -> Self {
        Self {
            model: text(&context.model),
            harness: text(&context.harness),
            task: text(&context.task),
            instruction_sha256: text(&context.instruction_sha256),
            client_session: text(&context.client_session),
            declared_at: time(context.declared_at.as_ref()),
        }
    }
}

/// A workspace's provenance session (R3.04).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProvenanceSession {
    /// `ses_` and 16 hex digits.
    pub session_id: String,
    /// The identity its records are bound to; `null` when none can be.
    pub identity: Option<String>,
    /// That identity's kind: `person` or `ci`.
    #[schemars(schema_with = "identity_kind_name_schema_or_null")]
    pub identity_kind: Option<String>,
    /// Its declared actor: `agent`, `executor` or `human`.
    #[schemars(schema_with = "session_actor_name_schema_or_null")]
    pub actor: Option<String>,
    /// Its agent label.
    pub agent: Option<String>,
    /// The context declared now.
    pub context: ProvenanceContext,
    /// When it started.
    pub started_at: Option<String>,
    /// When it ended; `null` while live.
    pub ended_at: Option<String>,
    /// How many records it has.
    pub records: u64,
    /// Commits recorded with no record, by a failure.
    pub missing_records: u64,
    /// Why its records are unbound: `no_default_identity`,
    /// `token_not_linked`, `token_revoked` or `identity_revoked`; `null`
    /// when they are bound.
    pub problem: Option<String>,
}

impl From<&v1::ProvenanceSession> for ProvenanceSession {
    fn from(session: &v1::ProvenanceSession) -> Self {
        Self {
            session_id: session.session_id.clone(),
            identity: text(&session.identity),
            identity_kind: name(identity_kind_name(session.identity_kind)),
            actor: name(session_actor_name(session.actor)),
            agent: text(&session.agent),
            context: session
                .context
                .as_ref()
                .map(ProvenanceContext::from)
                .unwrap_or_default(),
            started_at: time(session.started_at.as_ref()),
            ended_at: time(session.ended_at.as_ref()),
            records: session.records,
            missing_records: session.missing_records,
            problem: text(&session.problem),
        }
    }
}

/// A provenance record (R3.04, ADR 0058): the daemon's signed statement
/// that it recorded a commit in a workspace's session, under a declared
/// context. `signature` says whether it checked out at this read; the
/// declared fields are claims; policy decides whom to trust.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub struct ProvenanceRecord {
    /// SHA-256 of the canonical envelope.
    pub id: String,
    /// The commit the daemon recorded.
    pub commit_id: String,
    /// Its change, in hex.
    pub change_id: String,
    /// Its tree; `null` when conflicted.
    pub tree_id: Option<String>,
    /// The workspace.
    pub workspace_id: String,
    /// The provenance session.
    pub session_id: String,
    /// Its number in the session.
    pub sequence: u64,
    /// What recorded it: a snapshot kind, or `edit`.
    pub kind: String,
    /// The session's agent label, declared.
    pub agent: Option<String>,
    /// When the daemon recorded it.
    pub recorded_at: Option<String>,
    /// The declared context in effect then.
    pub context: ProvenanceContext,
    /// The signing key's id.
    pub key_id: String,
    /// `valid` when the envelope, its signature and the stored index check
    /// out at this read.
    #[schemars(schema_with = "signature_check_name_schema_or_null")]
    pub signature: Option<String>,
    /// The signing key's identity at `recorded_at`.
    pub identity: Option<KeyIdentity>,
    /// The payload, an in-toto Statement, as JSON (`show --envelope` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub statement: Option<serde_json::Value>,
    /// The canonical DSSE envelope, as JSON (`show --envelope` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub envelope: Option<serde_json::Value>,
}

impl From<&v1::ProvenanceRecord> for ProvenanceRecord {
    fn from(record: &v1::ProvenanceRecord) -> Self {
        let envelope: Option<serde_json::Value> = (!record.envelope.is_empty())
            .then(|| serde_json::from_slice(&record.envelope).ok())
            .flatten();
        let statement = envelope
            .as_ref()
            .and_then(|value| value.get("payload"))
            .and_then(serde_json::Value::as_str)
            .and_then(|payload| STANDARD.decode(payload).ok())
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        Self {
            id: record.id.clone(),
            commit_id: record.commit_id.clone(),
            change_id: record.change_id.clone(),
            tree_id: text(&record.tree_id),
            workspace_id: record.workspace_id.clone(),
            session_id: record.session_id.clone(),
            sequence: record.sequence,
            kind: record.kind.clone(),
            agent: text(&record.agent),
            recorded_at: time(record.recorded_at.as_ref()),
            context: record
                .context
                .as_ref()
                .map(ProvenanceContext::from)
                .unwrap_or_default(),
            key_id: record.key_id.clone(),
            signature: name(signature_check_name(record.signature)),
            identity: record.identity.as_ref().map(KeyIdentity::from),
            statement,
            envelope,
        }
    }
}

/// A job state's name: `queued`, `running`, `completed`, `cancelled` or
/// `expired` (R3.05).
pub fn job_state_name(state: i32) -> &'static str {
    match v1::JobState::try_from(state) {
        Ok(v1::JobState::Queued) => "queued",
        Ok(v1::JobState::Running) => "running",
        Ok(v1::JobState::Completed) => "completed",
        Ok(v1::JobState::Cancelled) => "cancelled",
        Ok(v1::JobState::Expired) => "expired",
        _ => UNKNOWN,
    }
}

/// A job outcome's name: `done`, `budget_exhausted` or `failed`; `None`
/// when unspecified (R3.05).
pub fn job_outcome_name(outcome: i32) -> Option<&'static str> {
    let name = match v1::JobOutcome::try_from(outcome) {
        Ok(v1::JobOutcome::Unspecified) => return None,
        Ok(v1::JobOutcome::Done) => "done",
        Ok(v1::JobOutcome::BudgetExhausted) => "budget_exhausted",
        Ok(v1::JobOutcome::Failed) => "failed",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// A job end reason's name: `queue_timeout`, `attempts` or `deadline`;
/// `None` when unspecified (R3.05).
pub fn job_end_reason_name(reason: i32) -> Option<&'static str> {
    let name = match v1::JobEndReason::try_from(reason) {
        Ok(v1::JobEndReason::Unspecified) => return None,
        Ok(v1::JobEndReason::QueueTimeout) => "queue_timeout",
        Ok(v1::JobEndReason::Attempts) => "attempts",
        Ok(v1::JobEndReason::Deadline) => "deadline",
        Err(_) => UNKNOWN,
    };
    Some(name)
}

/// The state a name from [`job_state_name`] stands for.
pub fn job_state_of(name: &str) -> Option<v1::JobState> {
    (1..=5)
        .filter_map(|value| v1::JobState::try_from(value).ok())
        .find(|state| job_state_name(*state as i32) == name)
}

names_schema! {
    job_state_name_schema = job_state_name, false;
    job_outcome_name_schema_or_null = job_outcome_name, true;
    job_end_reason_name_schema_or_null = job_end_reason_name, true;
}

fn duration_ms(value: Option<&prost_types::Duration>) -> Option<u64> {
    let value = value?;
    let ms = i128::from(value.seconds) * 1000 + i128::from(value.nanos) / 1_000_000;
    u64::try_from(ms).ok()
}

fn job_progress(progress: Option<&v1::JobProgress>) -> Option<Progress> {
    progress.map(|progress| Progress {
        phase: text(&progress.phase),
        done: progress.done,
        total: (progress.total > 0).then_some(progress.total),
        unit: text(&progress.unit),
    })
}

/// What a job requested (R3.05).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct JobSpec {
    /// The tree to check.
    pub tree_id: String,
    /// The evidence kinds expected.
    pub kinds: Vec<String>,
    /// The targets; empty for the whole check of each kind.
    pub targets: Vec<String>,
    /// One attempt's wall time, in milliseconds.
    pub wall_time_ms: Option<u64>,
    /// One attempt's CPU time, in milliseconds; `null` for no limit.
    pub cpu_time_ms: Option<u64>,
    /// Labels an executor must have, `key=value`.
    pub requirements: Vec<String>,
    /// Higher is claimed first.
    pub priority: i32,
    /// A commit with the tree, when submitted by revision.
    pub commit_id: Option<String>,
    /// The change, in hex, when submitted by revision.
    pub change_id: Option<String>,
}

/// The executor holding, or last holding, a job (R3.05).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct JobExecutor {
    /// Its declared name.
    pub name: String,
    /// Its declared version.
    pub version: Option<String>,
    /// The kinds it declared.
    pub kinds: Vec<String>,
    /// The labels it declared.
    pub labels: Vec<String>,
    /// The highest protocol version it declared.
    pub protocol_version: u32,
    /// The claim's `reef-agent`.
    pub agent: Option<String>,
    /// The remote token it claimed through; `null` on the socket.
    pub remote_token_id: Option<String>,
    /// The operator identity that token is linked to.
    pub identity: Option<String>,
}

/// What an attempt reported using (R3.05).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct JobUsage {
    /// Wall time, in milliseconds.
    pub wall_time_ms: u64,
    /// CPU time, in milliseconds.
    pub cpu_time_ms: u64,
}

/// One requested (kind, target) pair and the evidence that covers it
/// (R3.05).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct JobCoverage {
    /// The kind.
    pub kind: String,
    /// The target; `null` for the whole check.
    pub target: Option<String>,
    /// The latest accepted object of exactly this kind and target; `null`
    /// when none has arrived, or retention removed it.
    pub evidence_id: Option<String>,
    /// Its result.
    #[schemars(schema_with = "evidence_result_name_schema_or_null")]
    pub result: Option<String>,
    /// Whether it is fresh: stored by this job, and started no earlier than
    /// five minutes before the claim that sent it.
    pub fresh: bool,
    /// Its signature, checked again at this read.
    #[schemars(schema_with = "signature_check_name_schema_or_null")]
    pub signature: Option<String>,
}

/// An executor job (R3.05, ADR 0059): checks requested on a tree, claimed
/// by an executor outside Reef. Its state says nothing of pass or fail;
/// `coverage` and the evidence do.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Job {
    /// `job_` and 16 hex digits.
    pub id: String,
    /// Its repository.
    pub repo_id: String,
    /// Where it is.
    #[schemars(schema_with = "job_state_name_schema")]
    pub state: String,
    /// How much ran, once completed.
    #[schemars(schema_with = "job_outcome_name_schema_or_null")]
    pub outcome: Option<String>,
    /// Why the daemon ended it, once expired.
    #[schemars(schema_with = "job_end_reason_name_schema_or_null")]
    pub end_reason: Option<String>,
    /// What was requested.
    pub spec: JobSpec,
    /// The protocol version an executor needs.
    pub protocol_version: u32,
    /// The current or last attempt; 0 before the first claim.
    pub attempt: u32,
    /// Attempts before a lapsing claim expires it.
    pub max_attempts: u32,
    /// The submitter's `reef-agent`.
    pub requested_by_agent: Option<String>,
    /// The remote token that submitted it.
    pub requested_by_token: Option<String>,
    /// When it was submitted.
    pub submitted_at: Option<String>,
    /// When it expires unless claimed, while queued.
    pub queue_expires_at: Option<String>,
    /// When the current or last attempt was claimed.
    pub claimed_at: Option<String>,
    /// When the current claim lapses unless updated.
    pub lease_expires_at: Option<String>,
    /// When the daemon expires the running attempt.
    pub deadline_at: Option<String>,
    /// When it finished.
    pub finished_at: Option<String>,
    /// The current or last executor.
    pub executor: Option<JobExecutor>,
    /// The current or last attempt's progress.
    pub progress: Option<Progress>,
    /// What the completing attempt reported using.
    pub usage: Option<JobUsage>,
    /// Observed by the daemon over attempts, in milliseconds.
    pub elapsed_ms: u64,
    /// The executor's message, or the cancel reason.
    pub message: Option<String>,
    /// Evidence objects the job accepted.
    pub evidence_count: u32,
    /// One entry per requested pair (`job show` only).
    pub coverage: Vec<JobCoverage>,
}

impl From<&v1::Job> for Job {
    fn from(job: &v1::Job) -> Self {
        let spec = job.spec.clone().unwrap_or_default();
        let budget = spec.budget.unwrap_or_default();
        Self {
            id: job.job_id.clone(),
            repo_id: job.repo_id.clone(),
            state: job_state_name(job.state).to_owned(),
            outcome: name(job_outcome_name(job.outcome)),
            end_reason: name(job_end_reason_name(job.end_reason)),
            spec: JobSpec {
                tree_id: spec.tree_id,
                kinds: spec.kinds,
                targets: spec.targets,
                wall_time_ms: duration_ms(budget.wall_time.as_ref()),
                cpu_time_ms: duration_ms(budget.cpu_time.as_ref()),
                requirements: spec.requirements,
                priority: spec.priority,
                commit_id: text(&spec.commit_id),
                change_id: text(&spec.change_id),
            },
            protocol_version: job.protocol_version,
            attempt: job.attempt,
            max_attempts: job.max_attempts,
            requested_by_agent: text(&job.requested_by_agent),
            requested_by_token: text(&job.requested_by_token),
            submitted_at: time(job.submitted_at.as_ref()),
            queue_expires_at: time(job.queue_expires_at.as_ref()),
            claimed_at: time(job.claimed_at.as_ref()),
            lease_expires_at: time(job.lease_expires_at.as_ref()),
            deadline_at: time(job.deadline_at.as_ref()),
            finished_at: time(job.finished_at.as_ref()),
            executor: job.executor.as_ref().map(|executor| {
                let info = executor.info.clone().unwrap_or_default();
                JobExecutor {
                    name: info.name,
                    version: text(&info.version),
                    kinds: info.kinds,
                    labels: info.labels,
                    protocol_version: executor.protocol_version,
                    agent: text(&executor.agent),
                    remote_token_id: text(&executor.remote_token_id),
                    identity: text(&executor.identity),
                }
            }),
            progress: job_progress(job.progress.as_ref()),
            usage: job.usage.as_ref().map(|usage| JobUsage {
                wall_time_ms: usage.wall_time_ms,
                cpu_time_ms: usage.cpu_time_ms,
            }),
            elapsed_ms: job.elapsed_ms,
            message: text(&job.message),
            evidence_count: job.evidence_count,
            coverage: job
                .coverage
                .iter()
                .map(|entry| JobCoverage {
                    kind: entry.kind.clone(),
                    target: text(&entry.target),
                    evidence_id: text(&entry.evidence_id),
                    result: name(evidence_result_name(entry.result)),
                    fresh: entry.fresh,
                    signature: name(signature_check_name(entry.signature)),
                })
                .collect(),
        }
    }
}

/// What `reef job submit` did (R3.05).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct JobSubmitted {
    /// The job; with `--wait`, as it finished.
    pub job: Job,
    /// False when an identical open job was returned instead.
    pub created: bool,
}

/// A job as its `job` presence entry reports it (R3.05).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct JobPresence {
    /// The job's id.
    pub job_id: String,
    /// Its state.
    #[schemars(schema_with = "job_state_name_schema")]
    pub state: String,
    /// How much ran, once completed.
    #[schemars(schema_with = "job_outcome_name_schema_or_null")]
    pub outcome: Option<String>,
    /// Why the daemon ended it, once expired.
    #[schemars(schema_with = "job_end_reason_name_schema_or_null")]
    pub end_reason: Option<String>,
    /// The current or last attempt.
    pub attempt: u32,
    /// The current or last executor's name.
    pub executor: Option<String>,
    /// The kinds requested.
    pub kinds: Vec<String>,
    /// The tree.
    pub tree_id: String,
    /// The executor's last progress report.
    pub progress: Option<Progress>,
}

impl From<&v1::JobPresence> for JobPresence {
    fn from(job: &v1::JobPresence) -> Self {
        Self {
            job_id: job.job_id.clone(),
            state: job_state_name(job.state).to_owned(),
            outcome: name(job_outcome_name(job.outcome)),
            end_reason: name(job_end_reason_name(job.end_reason)),
            attempt: job.attempt,
            executor: text(&job.executor),
            kinds: job.kinds.clone(),
            tree_id: job.tree_id.clone(),
            progress: job_progress(job.progress.as_ref()),
        }
    }
}

/// What `reef provenance declare` did (R3.04).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProvenanceDeclared {
    /// The session, with its new context.
    pub session: ProvenanceSession,
    /// The snapshot taken first; `null` when nothing changed or with
    /// `--no-snapshot`.
    pub snapshot_op: Option<String>,
}
