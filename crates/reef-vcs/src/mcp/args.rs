//! Each tool's input: the request's field names, every field optional
//! unless a tool needs it, and unknown fields refused. A field's doc
//! comment is its schema `description`; the structs' own are not sent
//! (`rmcp` strips the root's).

#![expect(
    clippy::doc_markdown,
    reason = "field docs are the tools' schema descriptions, which name fields bare"
)]

use reef_api::v1;
use schemars::JsonSchema;
use serde::Deserialize;

/// `list_repositories`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ListRepositories {
    /// Repositories per page [default: 100, at most 1000].
    pub(super) page_size: Option<u32>,
    /// From the last result.
    pub(super) page_token: Option<String>,
}

/// `list_workspaces`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ListWorkspaces {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Workspaces of every registered repository instead of one.
    pub(super) all_repositories: Option<bool>,
    /// Only these states [default: active].
    pub(super) states: Option<Vec<State>>,
    /// Only workspaces with this owner.
    pub(super) owner: Option<String>,
    /// Workspaces per page [default: 50, at most 1000].
    pub(super) page_size: Option<u32>,
    /// From the last result.
    pub(super) page_token: Option<String>,
}

/// A workspace state.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum State {
    Active,
    Releasing,
    Released,
    Reclaimed,
}

impl From<State> for v1::WorkspaceState {
    fn from(state: State) -> Self {
        match state {
            State::Active => Self::Active,
            State::Releasing => Self::Releasing,
            State::Released => Self::Released,
            State::Reclaimed => Self::Reclaimed,
        }
    }
}

/// `get_workspace`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct GetWorkspace {
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// Snapshot the workspace's files first.
    pub(super) snapshot_first: Option<bool>,
    /// Walk the directory for its disk usage.
    pub(super) disk_usage: Option<bool>,
}

/// `get_status`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct GetStatus {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// Report the last snapshot instead of snapshotting the workspace first.
    pub(super) skip_snapshot: Option<bool>,
    /// List at most this many changed files [default: 100].
    pub(super) max_files: Option<u32>,
}

/// `list_changes`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ListChanges {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// A jj revset, such as `trunk()..@` [default: Reef's log set].
    pub(super) revset: Option<String>,
    /// Read as of this operation (id or prefix).
    pub(super) at_operation: Option<String>,
    /// Skip the snapshot; read the last one.
    pub(super) skip_snapshot: Option<bool>,
    /// Entries per page [default: 50, at most 1000].
    pub(super) page_size: Option<u32>,
    /// From the last result.
    pub(super) page_token: Option<String>,
}

/// `get_change`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct GetChange {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// A change or commit id, bookmark, or revset naming one commit (default: `@`, the workspace's change).
    pub(super) revision: Option<String>,
    /// Read as of this operation (id or prefix).
    pub(super) at_operation: Option<String>,
    /// Skip the snapshot; read the last one.
    pub(super) skip_snapshot: Option<bool>,
}

/// `get_diff`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct GetDiff {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// The old side [default: the new side's parents].
    pub(super) from_revision: Option<String>,
    /// The new side (default: `@`).
    pub(super) to_revision: Option<String>,
    /// Only these paths, repository-relative or absolute inside the scope.
    pub(super) paths: Option<Vec<String>>,
    /// `hunks`, `stat` (line counts), `summary` (paths) or `symbols` (changed symbols, call sites) [default: hunks].
    pub(super) format: Option<Format>,
    /// Only these symbols' lines.
    pub(super) expand: Option<Vec<String>>,
    /// Context lines [default: 3].
    pub(super) context_lines: Option<u32>,
    /// Hunk lines per file [default: 1000].
    pub(super) max_lines_per_file: Option<u32>,
    /// Read as of this operation (id or prefix).
    pub(super) at_operation: Option<String>,
    /// Skip the snapshot; read the last one.
    pub(super) skip_snapshot: Option<bool>,
    /// Files per page [default: 20, at most 1000].
    pub(super) page_size: Option<u32>,
    /// From the last result.
    pub(super) page_token: Option<String>,
}

/// A diff's form.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum Format {
    Summary,
    Stat,
    Hunks,
    Symbols,
}

impl From<Format> for v1::DiffFormat {
    fn from(format: Format) -> Self {
        match format {
            Format::Summary | Format::Symbols => Self::Summary,
            Format::Stat => Self::Stat,
            Format::Hunks => Self::Hunks,
        }
    }
}

/// `list_operations`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ListOperations {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Only operations for these workspaces (ids, prefixes, or paths).
    pub(super) workspaces: Option<Vec<String>>,
    /// Only operations touching these changes (hex or reverse-hex prefixes).
    pub(super) changes: Option<Vec<String>>,
    /// Only operations by these agents.
    pub(super) agents: Option<Vec<String>>,
    /// Only operations of these kinds.
    pub(super) kinds: Option<Vec<Kind>>,
    /// Include what each operation changed.
    pub(super) include_effects: Option<bool>,
    /// Operations per page, newest first [default: 50, at most 1000].
    pub(super) page_size: Option<u32>,
    /// From the last result.
    pub(super) page_token: Option<String>,
}

/// `query_changes`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct QueryChanges {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Files or directories, repository-relative.
    pub(super) paths: Option<Vec<String>>,
    /// Symbol, `Class#method()` suffix, or name.
    pub(super) symbols: Option<Vec<String>>,
    /// Only changes these agents wrote.
    pub(super) agents: Option<Vec<String>>,
    /// Only these outcomes.
    pub(super) outcomes: Option<Vec<Outcome>>,
    /// Only changes written since (RFC 3339).
    pub(super) since: Option<String>,
    /// Changes per page [default: 20, at most 1000].
    pub(super) page_size: Option<u32>,
    /// From the last result.
    pub(super) page_token: Option<String>,
}

/// What became of a change.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub(super) enum Outcome {
    Open,
    Landed,
    Abandoned,
    Undone,
}

/// An operation kind, by its `reef.kind` name.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub(super) enum Kind {
    Snapshot,
    CreateWorkspace,
    ReleaseWorkspace,
    MoveWorkspace,
    Describe,
    Abandon,
    Undo,
    Edit,
    ImportRefs,
    External,
    Other,
}

impl From<Kind> for v1::OperationKind {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Snapshot => Self::Snapshot,
            Kind::CreateWorkspace => Self::CreateWorkspace,
            Kind::ReleaseWorkspace => Self::ReleaseWorkspace,
            Kind::MoveWorkspace => Self::MoveWorkspace,
            Kind::Describe => Self::Describe,
            Kind::Abandon => Self::Abandon,
            Kind::Undo => Self::Undo,
            Kind::Edit => Self::Edit,
            Kind::ImportRefs => Self::ImportRefs,
            Kind::External => Self::External,
            Kind::Other => Self::Other,
        }
    }
}

/// `get_operation`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct GetOperation {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// An operation id or unique prefix.
    pub(super) operation_id: String,
}

/// `read_file`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadFile {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// The file, repository-relative or absolute inside the scope.
    pub(super) path: String,
    /// The revision to read at (default: `@`).
    pub(super) revision: Option<String>,
    /// The first byte to read [default: 0].
    pub(super) offset: Option<u64>,
    /// At most this many bytes [default: 65536, at most 1048576].
    pub(super) limit: Option<u64>,
    /// Read as of this operation (id or prefix).
    pub(super) at_operation: Option<String>,
    /// Skip the snapshot; read the last one.
    pub(super) skip_snapshot: Option<bool>,
}

/// `list_files`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ListFiles {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// The directory, repository-relative or absolute inside the scope [default: the root].
    pub(super) path: Option<String>,
    /// The revision to list at (default: `@`).
    pub(super) revision: Option<String>,
    /// List everything under the directory, not only its entries.
    pub(super) recursive: Option<bool>,
    /// Read as of this operation (id or prefix).
    pub(super) at_operation: Option<String>,
    /// Skip the snapshot; read the last one.
    pub(super) skip_snapshot: Option<bool>,
    /// Entries per page [default: 200, at most 1000].
    pub(super) page_size: Option<u32>,
    /// From the last result.
    pub(super) page_token: Option<String>,
}

/// `wait_for_operations`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct WaitForOperations {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Only operations for these workspaces (ids, prefixes, or paths).
    pub(super) workspaces: Option<Vec<String>>,
    /// Only operations touching these changes (hex or reverse-hex prefixes).
    pub(super) changes: Option<Vec<String>>,
    /// Only operations by these agents.
    pub(super) agents: Option<Vec<String>>,
    /// Only operations of these kinds.
    pub(super) kinds: Option<Vec<Kind>>,
    /// Wait for operations after this one (id or prefix), such as the last result's last_operation_id [default: the newest now].
    pub(super) after_operation: Option<String>,
    /// Give up after this many seconds, 0 to check once [default: 30, at most 300].
    pub(super) timeout_seconds: Option<u64>,
    /// Include what each operation changed.
    pub(super) include_effects: Option<bool>,
}

/// `create_workspace`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateWorkspace {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Edit this existing change (id, bookmark, or a revset naming one commit).
    pub(super) change: Option<String>,
    /// Start a new change on this revision [default: the repository's default start].
    pub(super) new_on: Option<String>,
    /// What the workspace is for, shown in list_workspaces.
    pub(super) purpose: Option<String>,
    /// When a declared dependency store is not built [default: fail].
    pub(super) missing_stores: Option<Missing>,
}

/// What to do about stores that are not built.
#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(super) enum Missing {
    Fail,
    Skip,
}

impl From<Missing> for v1::MissingStores {
    fn from(missing: Missing) -> Self {
        match missing {
            Missing::Fail => Self::Fail,
            Missing::Skip => Self::Skip,
        }
    }
}

/// `snapshot_workspace`.
#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct SnapshotWorkspace {
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// The snapshot operation's description [default: Reef's].
    pub(super) description: Option<String>,
}

/// `move_workspace`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct MoveWorkspace {
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: String,
    /// Edit this existing change (id, bookmark, or a revset naming one commit).
    pub(super) change: Option<String>,
    /// Start a new change on this revision [default: the repository's default start].
    pub(super) new_on: Option<String>,
    /// When a declared dependency store is not built [default: fail].
    pub(super) missing_stores: Option<Missing>,
}

/// `release_workspace`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct ReleaseWorkspace {
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: String,
}

/// `describe_change`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct DescribeChange {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// The change: a change or commit id, bookmark, or revset naming one commit (default: `@`).
    pub(super) revision: Option<String>,
    /// The new description; empty clears it.
    pub(super) description: String,
    /// Refuse unless the revision is still this commit (full id).
    pub(super) expected_commit_id: Option<String>,
}

/// `abandon_change`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct AbandonChange {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// The change: a change or commit id, or a revset naming one commit.
    pub(super) revision: String,
    /// Refuse unless the revision is still this commit (full id).
    pub(super) expected_commit_id: Option<String>,
}

/// `undo_operation`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct UndoOperation {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// The operation to revert (id or unique prefix; list_operations lists them).
    pub(super) operation_id: String,
}

/// `edit_files`.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct EditFiles {
    /// Repository: id, prefix, or absolute path in it.
    pub(super) repo: Option<String>,
    /// Workspace: id, prefix, or absolute path in it.
    pub(super) workspace: Option<String>,
    /// Rewrite this change (`@` for the workspace's); give this or new_on.
    pub(super) revision: Option<String>,
    /// Create a new change on this revision with the edits; give this or revision.
    pub(super) new_on: Option<String>,
    /// The edits, 1 to 1000, applied in order.
    pub(super) edits: Vec<Edit>,
    /// Refuse unless the revision is still this commit (full id).
    pub(super) expected_commit_id: Option<String>,
    /// The change's new description [default: unchanged, or empty for new_on].
    pub(super) description: Option<String>,
}

/// One file's edit: at most one of content, content_base64, replace, delete and copy_from; none only with executable.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Edit {
    /// The file, repository-relative or absolute inside the scope.
    pub(super) path: String,
    /// The whole new content, as text.
    pub(super) content: Option<String>,
    /// The whole new content, as standard padded base64 (binary files).
    pub(super) content_base64: Option<String>,
    /// Replace text in the file.
    pub(super) replace: Option<Replace>,
    /// Delete the file.
    pub(super) delete: Option<bool>,
    /// Copy this file; with a delete of the old path, a rename.
    pub(super) copy_from: Option<String>,
    /// Set or clear the executable bit.
    pub(super) executable: Option<bool>,
}

/// A text replacement.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(super) struct Replace {
    /// The text to find; it must occur exactly once unless `all`.
    pub(super) old_text: String,
    /// What to put in its place.
    pub(super) new_text: String,
    /// Replace every occurrence.
    pub(super) all: Option<bool>,
}
