//! The tools: one row of [`TOOLS`] per tool, named after the RPC it calls
//! so the daemon's hints, which name RPCs and request fields, name the tool
//! and argument to use. Annotations follow rules from the schema (ADR
//! 0035): read-only exactly for READ RPCs, idempotent for a write exactly
//! when its RPC is `IDEMPOTENT`, and non-destructive only for the writes
//! that replace nothing. [`NOT_TOOLS`] says why every other RPC is not a
//! tool; a test fails until each v1 method is in one or the other.

use std::future::Future;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reef_api::v1::{
    self, AbandonChangeRequest, CreateWorkspaceRequest, DescribeChangeRequest, EditFilesRequest,
    GetChangeRequest, GetDiffRequest, GetStatusRequest, GetWorkspaceRequest, ListChangesRequest,
    ListFilesRequest, ListOperationsRequest, ListRepositoriesRequest, ListWorkspacesRequest,
    MoveWorkspaceRequest, OperationFilter, QueryChangesRequest, ReadFileRequest,
    UndoOperationRequest, create_workspace_request, edit_files_request, file_edit,
    move_workspace_request,
};
use reef_client::brief;
use reef_client::json::{
    self, Abandoned, Created, Described, Diff, Edited, ErrorObject, FileEntry, FileRead, History,
    Inspected, LogEntry, Moved, Operation, Released, RepoStatus, Repository, Snapshotted, Undone,
    Workspace,
};
use rmcp::handler::server::common::{schema_for_empty_input, schema_for_input};
use rmcp::model::{JsonObject, Tool, ToolAnnotations};
use schemars::JsonSchema;
use serde_json::{Map, Value};

use super::results::{
    ChangeAt, Changes, DiffPage, FileRange, Files, HistoryPage, Operations, Repositories, Waited,
    Workspaces, token,
};
use super::{Call, ToolError, args, parse, to_value, wait};
use crate::cli::changes::workspace_after;
use crate::cli::repo::missing;
use crate::cli::scope::{self, Scope, workspace_arg};

/// A tool's body.
pub(super) type ToolFn =
    for<'a> fn(
        &'a Call<'a>,
        Map<String, Value>,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + 'a>>;

/// One tool.
pub(super) struct ToolDef {
    pub(super) name: &'static str,
    pub(super) title: &'static str,
    /// The RPC it calls, `/reef.api.v1.<Service>/<Rpc>`, whose access level
    /// decides `readOnlyHint`.
    pub(super) rpc: &'static str,
    /// One to three sentences in the task's words, ending in the result.
    pub(super) description: &'static str,
    /// `destructiveHint` for a write: false only when it replaces nothing.
    pub(super) destructive: bool,
    /// `idempotentHint` for a write: its RPC's `IDEMPOTENT`.
    pub(super) idempotent: bool,
    pub(super) schema: fn() -> Arc<JsonObject>,
    /// The result's schema, which `reef schema mcp` publishes and
    /// `tools/list` does not declare (R2.24, ADR 0050).
    pub(super) output: fn() -> Value,
    pub(super) call: ToolFn,
}

impl ToolDef {
    /// Whether its RPC only reads (`ACCESS_READ`).
    pub(super) fn reads(&self) -> bool {
        reef_api::access(self.rpc) == Some(v1::Access::Read)
    }

    /// Whether a call is run again after the daemon went away during it:
    /// reads, but not the wait, whose retry would take a new baseline and
    /// miss what happened before the outage.
    pub(super) fn retries(&self) -> bool {
        self.reads() && self.name != "wait_for_operations"
    }

    fn annotations(&self) -> ToolAnnotations {
        let annotations = ToolAnnotations::new()
            .read_only(self.reads())
            .open_world(false);
        if self.reads() {
            annotations
        } else {
            annotations
                .destructive(self.destructive)
                .idempotent(self.idempotent)
        }
    }

    fn tool(&self) -> Tool {
        Tool::new(self.name, self.description, (self.schema)())
            .with_title(self.title)
            .with_annotations(self.annotations())
    }
}

/// `tools/list`.
pub(super) fn list() -> Vec<Tool> {
    TOOLS.iter().map(ToolDef::tool).collect()
}

/// Every tool as `tools/list` gives it, with its `outputSchema`, for `reef
/// schema mcp` (R2.24). `tools/list` leaves the output schemas out: every
/// session pays for the list in tokens (#116), the descriptions name each
/// result's shape, and a declared schema would bind error results too.
pub(super) fn definitions() -> Vec<Value> {
    TOOLS
        .iter()
        .map(|def| {
            let mut tool = serde_json::to_value(def.tool()).unwrap_or_default();
            if let Value::Object(tool) = &mut tool {
                tool.insert("outputSchema".to_owned(), (def.output)());
            }
            tool
        })
        .collect()
}

/// A tool's output schema: self-contained, as a tool definition cannot
/// point into another document, and described as the result serializes,
/// `null` included.
pub(super) fn output<T: JsonSchema + 'static>() -> Value {
    let generator = schemars::SchemaGenerator::new(reef_client::schema::settings());
    let mut schema = Value::from(generator.into_root_schema_for::<T>());
    if let Some(Value::Object(defs)) = schema.get_mut("$defs") {
        reef_client::schema::describe_brief(defs);
    }
    // A brief view at the root (`get_status`), or flattened into it
    // (`get_diff`'s page), is not among the `$defs`.
    let title = schema
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_owned);
    match title.as_deref() {
        Some("mcp.DiffPage") => reef_client::schema::describe_view_root(&mut schema, "Diff"),
        Some(view) if view.starts_with(reef_client::schema::BRIEF_PREFIX) => {
            let full = view
                .trim_start_matches(reef_client::schema::BRIEF_PREFIX)
                .to_owned();
            reef_client::schema::describe_view_root(&mut schema, &full);
        }
        _ => {}
    }
    compact(&mut schema, true);
    reef_client::schema::strip_doc_links(&mut schema);
    schema
}

/// A tool's input schema; `T` always derives an object. Compacted, since
/// every session pays for the tool list in tokens.
fn schema<T: JsonSchema + 'static>() -> Arc<JsonObject> {
    let Ok(schema) = schema_for_input::<T>() else {
        return schema_for_empty_input();
    };
    let mut schema = Value::Object((*schema).clone());
    compact(&mut schema, false);
    inline_enums(&mut schema);
    match schema {
        Value::Object(object) => Arc::new(object),
        _ => schema_for_empty_input(),
    }
}

/// Puts each enum of `$defs` (a kind, a state) where it is referenced,
/// without its type's doc comment, and drops `$defs` once empty.
fn inline_enums(schema: &mut Value) {
    fn inline(value: &mut Value, enums: &Map<String, Value>) {
        match value {
            Value::Object(object) => {
                let target = object
                    .get("$ref")
                    .and_then(Value::as_str)
                    .and_then(|path| path.strip_prefix("#/$defs/"))
                    .and_then(|name| enums.get(name));
                if let Some(Value::Object(def)) = target.cloned() {
                    object.remove("$ref");
                    object.extend(def.into_iter().filter(|(key, _)| key != "description"));
                }
                object.values_mut().for_each(|value| inline(value, enums));
            }
            Value::Array(items) => items.iter_mut().for_each(|value| inline(value, enums)),
            _ => {}
        }
    }
    let Some(Value::Object(defs)) = schema.get_mut("$defs") else {
        return;
    };
    let names: Vec<String> = defs
        .iter()
        .filter(|(_, def)| def.get("enum").is_some())
        .map(|(name, _)| name.clone())
        .collect();
    let enums: Map<String, Value> = names
        .iter()
        .filter_map(|name| defs.remove(name).map(|def| (name.clone(), def)))
        .collect();
    if defs.is_empty()
        && let Value::Object(object) = schema
    {
        object.remove("$defs");
    }
    inline(schema, &enums);
}

/// Drops what a model does not need from a derived schema: `$schema`,
/// integer `format`s and their `minimum: 0`, and, unless `keep_null`,
/// `null` beside an optional field's type (an input field is optional by
/// not being `required`; an output prints `null`). `schema` is a schema
/// object; the rules apply to schemas only, never to the maps of them
/// (`properties`, `$defs`), whose keys are field names.
fn compact(schema: &mut Value, keep_null: bool) {
    let Value::Object(object) = schema else {
        return;
    };
    object.remove("$schema");
    if let Some(Value::Array(types)) = object.get_mut("type")
        && !keep_null
    {
        types.retain(|kind| kind != "null");
        if types.len() == 1 {
            let kind = types.remove(0);
            object.insert("type".to_owned(), kind);
        }
    }
    if matches!(
        object.get("type").and_then(Value::as_str),
        Some("integer" | "number")
    ) && object.remove("format").is_some()
        && object.get("minimum") == Some(&Value::from(0))
    {
        object.remove("minimum");
    }
    if let Some(Value::Array(any)) = object.get("anyOf")
        && !keep_null
        && let [only, null] = any.as_slice()
        && null == &serde_json::json!({"type": "null"})
        && let Value::Object(only) = only.clone()
    {
        object.remove("anyOf");
        object.extend(only);
    }
    for (key, value) in object.iter_mut() {
        match (key.as_str(), value) {
            ("properties" | "$defs", Value::Object(schemas)) => {
                for schema in schemas.values_mut() {
                    compact(schema, keep_null);
                }
            }
            ("items" | "additionalProperties", value) => compact(value, keep_null),
            ("anyOf" | "oneOf" | "allOf" | "prefixItems", Value::Array(schemas)) => {
                for schema in schemas.iter_mut() {
                    compact(schema, keep_null);
                }
            }
            _ => {}
        }
    }
}

/// Every tool, in the order `tools/list` gives them.
pub(super) const TOOLS: &[ToolDef] = &[
    ToolDef {
        name: "list_repositories",
        title: "List repositories",
        rpc: "/reef.api.v1.RepositoryService/ListRepositories",
        description: "List the repositories registered with Reef, oldest first. Returns {repositories, next_page_token}.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::ListRepositories>,
        output: output::<Repositories>,
        call: list_repositories,
    },
    ToolDef {
        name: "list_workspaces",
        title: "List workspaces",
        rpc: "/reef.api.v1.WorkspaceService/ListWorkspaces",
        description: "List a repository's workspaces, newest first: each is a directory leased to one agent, editing one change, with how many files it changed. Returns {workspaces, next_page_token}.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::ListWorkspaces>,
        output: output::<Workspaces<'static>>,
        call: list_workspaces,
    },
    ToolDef {
        name: "get_workspace",
        title: "Get workspace",
        rpc: "/reef.api.v1.WorkspaceService/GetWorkspace",
        description: "Show a workspace: its directory, change, lease and recent snapshots. Returns the workspace as `reef ws inspect -o json` prints it.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::GetWorkspace>,
        output: output::<Inspected>,
        call: get_workspace,
    },
    ToolDef {
        name: "get_status",
        title: "Get status",
        rpc: "/reef.api.v1.StatusService/GetStatus",
        description: "Show what the workspace changes, after a snapshot, and the repository's state. Returns {repository_status, workspace_status}.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::GetStatus>,
        output: output::<brief::RepoStatus<'static>>,
        call: get_status,
    },
    ToolDef {
        name: "list_changes",
        title: "List changes",
        rpc: "/reef.api.v1.ChangeService/ListChanges",
        description: "List changes, newest first, as `reef log` does. Returns {entries, at_operation, next_page_token}.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::ListChanges>,
        output: output::<Changes>,
        call: list_changes,
    },
    ToolDef {
        name: "get_change",
        title: "Get change",
        rpc: "/reef.api.v1.ChangeService/GetChange",
        description: "Show one change: its ids, description, parents and state, without its files (get_diff has them). Returns {change, at_operation}.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::GetChange>,
        output: output::<ChangeAt>,
        call: get_change,
    },
    ToolDef {
        name: "get_diff",
        title: "Get diff",
        rpc: "/reef.api.v1.DiffService/GetDiff",
        description: "A change's diff against its parents, or two revisions' diff, a page of files at a time. Returns {from_commit_ids, to_commit_id, files, symbol_summary, next_page_token}.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::GetDiff>,
        output: output::<DiffPage<'static>>,
        call: get_diff,
    },
    ToolDef {
        name: "list_operations",
        title: "List operations",
        rpc: "/reef.api.v1.OperationService/ListOperations",
        description: "List the repository's operation log, newest first: every snapshot, describe, edit and workspace change, by whom. Returns {operations, next_page_token}.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::ListOperations>,
        output: output::<Operations>,
        call: list_operations,
    },
    ToolDef {
        name: "query_changes",
        title: "Query changes",
        rpc: "/reef.api.v1.HistoryService/QueryChanges",
        description: "Which changes touched these files or symbols: who wrote them, when, and whether they landed. Newest first. Returns {changes, coverage, next_page_token}.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::QueryChanges>,
        output: output::<HistoryPage>,
        call: query_changes,
    },
    ToolDef {
        name: "get_operation",
        title: "Get operation",
        rpc: "/reef.api.v1.OperationService/GetOperation",
        description: "Show one operation and what it changed (effects). Returns the operation.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::GetOperation>,
        output: output::<Operation>,
        call: get_operation,
    },
    ToolDef {
        name: "read_file",
        title: "Read file",
        rpc: "/reef.api.v1.FileService/ReadFile",
        description: "Read a file at any revision, one range at a time, without a directory; pass the first range's at_operation on to read one version. Returns {path, size, content (text) or content_base64, offset, next_offset}; next_offset is null at the end.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::ReadFile>,
        output: output::<FileRange>,
        call: read_file,
    },
    ToolDef {
        name: "list_files",
        title: "List files",
        rpc: "/reef.api.v1.FileService/ListFiles",
        description: "List a directory at any revision. Returns {entries, commit_id, at_operation, next_page_token}.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::ListFiles>,
        output: output::<Files>,
        call: list_files,
    },
    ToolDef {
        name: "wait_for_operations",
        title: "Wait for operations",
        rpc: "/reef.api.v1.OperationService/SubscribeOperations",
        description: "Wait until operations matching the filter are recorded (another agent's describe, edit or snapshot) or the timeout passes; over 60 s may hit the client's tool timeout. Returns {operations (oldest first, at most 100), last_operation_id, timed_out}; pass last_operation_id back as after_operation for the next ones.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::WaitForOperations>,
        output: output::<Waited>,
        call: wait::run,
    },
    ToolDef {
        name: "create_workspace",
        title: "Create workspace",
        rpc: "/reef.api.v1.WorkspaceService/CreateWorkspace",
        description: "Create a workspace: a copy-on-write directory on a new change (or an existing one), snapshotted as it is written. Its lease is kept alive while the server runs; release_workspace it when done, or it is reclaimed with a final snapshot about 35 minutes after the server stops. Returns the workspace and its path.",
        destructive: false,
        idempotent: false,
        schema: schema::<args::CreateWorkspace>,
        output: output::<Created>,
        call: create_workspace,
    },
    ToolDef {
        name: "snapshot_workspace",
        title: "Snapshot workspace",
        rpc: "/reef.api.v1.WorkspaceService/SnapshotWorkspace",
        description: "Record the workspace's files now, as its change's new commit. Returns {changed, commit_id, operation, workspace}.",
        destructive: false,
        idempotent: true,
        schema: schema::<args::SnapshotWorkspace>,
        output: output::<Snapshotted>,
        call: snapshot_workspace,
    },
    ToolDef {
        name: "move_workspace",
        title: "Move workspace",
        rpc: "/reef.api.v1.WorkspaceService/MoveWorkspace",
        description: "Point a workspace at another change, or a new change on a revision, after a final snapshot of the one it leaves; its files are checked out again. Returns the workspace afterwards.",
        destructive: true,
        idempotent: false,
        schema: schema::<args::MoveWorkspace>,
        output: output::<Moved>,
        call: move_workspace,
    },
    ToolDef {
        name: "release_workspace",
        title: "Release workspace",
        rpc: "/reef.api.v1.WorkspaceService/ReleaseWorkspace",
        description: "Take a final snapshot of a workspace and remove its directory; its change stays in the repository. Returns {id, final_op_id, snapshot_taken, freed_bytes}.",
        destructive: true,
        idempotent: false,
        schema: schema::<args::ReleaseWorkspace>,
        output: output::<Released>,
        call: release_workspace,
    },
    ToolDef {
        name: "describe_change",
        title: "Describe change",
        rpc: "/reef.api.v1.ChangeService/DescribeChange",
        description: "Set a change's description (its commit message). Returns {change, operation, workspace}; operation is null when the description was already that.",
        destructive: true,
        idempotent: false,
        schema: schema::<args::DescribeChange>,
        output: output::<Described>,
        call: describe_change,
    },
    ToolDef {
        name: "abandon_change",
        title: "Abandon change",
        rpc: "/reef.api.v1.ChangeService/AbandonChange",
        description: "Abandon a change: its descendants move onto its parents. Returns {abandoned_commit_id, operation, workspace}.",
        destructive: true,
        idempotent: false,
        schema: schema::<args::AbandonChange>,
        output: output::<Abandoned>,
        call: abandon_change,
    },
    ToolDef {
        name: "undo_operation",
        title: "Undo operation",
        rpc: "/reef.api.v1.OperationService/UndoOperation",
        description: "Revert what one operation did, as a new operation (list_operations lists them). Returns {operation, snapshot, undone_operation_id, workspace}.",
        destructive: true,
        idempotent: false,
        schema: schema::<args::UndoOperation>,
        output: output::<Undone>,
        call: undo_operation,
    },
    ToolDef {
        name: "edit_files",
        title: "Edit files",
        rpc: "/reef.api.v1.FileService/EditFiles",
        description: "Write, replace text in, delete, copy or chmod files in a change (revision) or a new change (new_on), as one operation, without a directory; workspaces on the change get the new files. Rename: copy_from the old path, and delete it, in one call. Returns {change, operation, workspace}.",
        destructive: true,
        idempotent: false,
        schema: schema::<args::EditFiles>,
        output: output::<Edited>,
        call: edit_files,
    },
];

/// Every v1 method that is not a tool, and why.
#[cfg_attr(not(test), expect(dead_code, reason = "the test reads it"))]
pub(super) const NOT_TOOLS: &[(&str, &str)] = &[
    (
        "/reef.api.v1.DaemonService/GetDaemonStatus",
        "for a person running the CLI",
    ),
    (
        "/reef.api.v1.DaemonService/CreateWebSession",
        "ADMIN; a person runs reef ui",
    ),
    (
        "/reef.api.v1.RemoteService/CreateRemoteToken",
        "ADMIN; a person issues tokens",
    ),
    (
        "/reef.api.v1.RemoteService/ListRemoteTokens",
        "ADMIN; a person issues tokens",
    ),
    (
        "/reef.api.v1.RemoteService/RotateRemoteToken",
        "ADMIN; a person issues tokens",
    ),
    (
        "/reef.api.v1.RemoteService/RevokeRemoteToken",
        "ADMIN; a person issues tokens",
    ),
    (
        "/reef.api.v1.RepositoryService/GetRepository",
        "get_status names the repository",
    ),
    (
        "/reef.api.v1.RepositoryService/RegisterRepository",
        "ADMIN; a call naming a checkout registers it",
    ),
    ("/reef.api.v1.RepositoryService/ForgetRepository", "ADMIN"),
    (
        "/reef.api.v1.WorkspaceService/RenewLease",
        "the server heartbeats",
    ),
    (
        "/reef.api.v1.PresenceService/ReportPresence",
        "the server reports its calls' activity itself",
    ),
    (
        "/reef.api.v1.PresenceService/ClearPresence",
        "the server clears its entries at exit",
    ),
    (
        "/reef.api.v1.PresenceService/SubscribePresence",
        "a stream for live views; wait_for_operations waits",
    ),
    ("/reef.api.v1.MaintenanceService/CollectGarbage", "ADMIN"),
    (
        "/reef.api.v1.MaintenanceService/Backup",
        "ADMIN; Backup.dest is a way out of a sandbox (S-07)",
    ),
    ("/reef.api.v1.MaintenanceService/Quiesce", "ADMIN"),
    ("/reef.api.v1.MaintenanceService/Resume", "ADMIN"),
    (
        "/reef.api.v1.StoreService/BuildStore",
        "ADMIN; runs a command outside a sandbox (S-07)",
    ),
    (
        "/reef.api.v1.StoreService/ListStores",
        "for a person running the CLI",
    ),
    (
        "/reef.api.v1.SymbolService/IndexSymbols",
        "tools/list is at its budget; R2.15 decides symbol tools (R2.14)",
    ),
    (
        "/reef.api.v1.SymbolService/UploadScipDocuments",
        "an indexer's output, uploaded by CI or `reef symbols upload`",
    ),
    (
        "/reef.api.v1.SymbolService/ListSymbols",
        "tools/list is at its budget; R2.15 decides symbol tools (R2.14)",
    ),
];

/// A tool body: `$body` with `$args` parsed as `$ty`.
macro_rules! tool {
    ($name:ident, $ty:ty, |$call:ident, $args:ident| $body:expr) => {
        fn $name<'a>(
            $call: &'a Call<'a>,
            arguments: Map<String, Value>,
        ) -> Pin<Box<dyn Future<Output = Result<Value, ToolError>> + Send + 'a>> {
            Box::pin(async move {
                let $args: $ty = parse(arguments)?;
                $body
            })
        }
    };
}

/// A page size as the request takes it, `default` when not given or 0.
fn page(size: Option<u32>, default: i32) -> i32 {
    size.filter(|size| *size > 0)
        .map_or(default, |size| i32::try_from(size).unwrap_or(i32::MAX))
}

/// `path` repository-relative: an absolute path inside the scope's
/// directory is made relative to it; anything else goes to the daemon as
/// given, which refuses what is not repository-relative. An MCP caller has
/// no current directory the server can see, so relative paths are never
/// relative to one.
fn repo_path(scope: &Scope, path: String) -> String {
    if Path::new(&path).is_absolute() {
        scope::inside(&scope.dir, Path::new(&path)).unwrap_or(path)
    } else {
        path
    }
}

/// `@` unless given.
fn at(revision: Option<String>) -> String {
    revision.unwrap_or_else(|| "@".to_owned())
}

/// Both of two exclusive inputs given.
fn both(first: &str, second: &str) -> ToolError {
    ToolError::invalid(
        "",
        format!("{first} and {second} were both given"),
        &format!("Give {first} or {second}, not both."),
    )
}

tool!(list_repositories, args::ListRepositories, |call, args| {
    let ctx = call.ctx;
    let response = ctx
        .call(ctx.client.list_repositories_page(ListRepositoriesRequest {
            page_size: page(args.page_size, 100),
            page_token: args.page_token.unwrap_or_default(),
        }))
        .await?;
    to_value(&Repositories {
        repositories: response.repositories.iter().map(Repository::from).collect(),
        next_page_token: token(response.next_page_token),
    })
});

tool!(list_workspaces, args::ListWorkspaces, |call, args| {
    let ctx = call.ctx;
    let all_repositories = args.all_repositories.unwrap_or(false);
    let repo = if all_repositories {
        String::new()
    } else {
        call.scope(args.repo.as_ref(), None).await?.repo_id
    };
    let states = args
        .states
        .unwrap_or_else(|| vec![args::State::Active])
        .into_iter()
        .map(|state| v1::WorkspaceState::from(state).into())
        .collect();
    let response = ctx
        .call(ctx.client.list_workspaces_page(ListWorkspacesRequest {
            repo,
            states,
            owner: args.owner.unwrap_or_default(),
            page_size: page(args.page_size, 50),
            page_token: args.page_token.unwrap_or_default(),
            changed_file_counts: true,
            at_operation: String::new(),
        }))
        .await?;
    let workspaces: Vec<Workspace> = response.workspaces.iter().map(Workspace::from).collect();
    to_value(&Workspaces {
        // A page across repositories names each one's: other pages may hold
        // the others.
        workspaces: if all_repositories {
            brief::workspaces_with_repo(&workspaces)
        } else {
            brief::workspaces(&workspaces)
        },
        next_page_token: token(response.next_page_token),
    })
});

tool!(get_workspace, args::GetWorkspace, |call, args| {
    let ctx = call.ctx;
    let (scope, ws) = call.workspace(args.workspace.as_ref()).await?;
    let response = ctx
        .call(ctx.client.get_workspace(GetWorkspaceRequest {
            repo: scope.repo_id,
            workspace: ws.id,
            snapshot_first: args.snapshot_first.unwrap_or(false),
            skip_disk_usage: !args.disk_usage.unwrap_or(false),
            at_operation: String::new(),
        }))
        .await?;
    to_value(&Inspected::from_response(&response).ok_or_else(|| missing("workspace"))?)
});

tool!(get_status, args::GetStatus, |call, args| {
    let ctx = call.ctx;
    let scope = call
        .scope(args.repo.as_ref(), args.workspace.as_ref())
        .await?;
    let request = GetStatusRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        skip_snapshot: args.skip_snapshot.unwrap_or(false),
        max_files: page(args.max_files, 100),
        at_operation: String::new(),
    };
    let response = ctx.call(Box::pin(ctx.client.get_status(request))).await?;
    to_value(&brief::RepoStatus::from(&RepoStatus::from(&response)))
});

tool!(list_changes, args::ListChanges, |call, args| {
    let ctx = call.ctx;
    let scope = call
        .scope(args.repo.as_ref(), args.workspace.as_ref())
        .await?;
    let response = ctx
        .call(ctx.client.list_changes_page(ListChangesRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revset: args.revset.unwrap_or_default(),
            at_operation: args.at_operation.unwrap_or_default(),
            skip_snapshot: args.skip_snapshot.unwrap_or(false),
            page_size: page(args.page_size, 50),
            page_token: args.page_token.unwrap_or_default(),
        }))
        .await?;
    to_value(&Changes {
        entries: response.entries.iter().map(LogEntry::from).collect(),
        at_operation: response.at_operation,
        next_page_token: token(response.next_page_token),
    })
});

tool!(get_change, args::GetChange, |call, args| {
    let ctx = call.ctx;
    let scope = call
        .scope(args.repo.as_ref(), args.workspace.as_ref())
        .await?;
    let response = ctx
        .call(ctx.client.get_change(GetChangeRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision: at(args.revision),
            at_operation: args.at_operation.unwrap_or_default(),
            skip_snapshot: args.skip_snapshot.unwrap_or(false),
        }))
        .await?;
    to_value(&ChangeAt {
        change: response.change.as_ref().map(json::Change::from),
        at_operation: response.at_operation,
    })
});

tool!(get_diff, args::GetDiff, |call, args| {
    let ctx = call.ctx;
    let scope = call
        .scope(args.repo.as_ref(), args.workspace.as_ref())
        .await?;
    let paths = args
        .paths
        .unwrap_or_default()
        .into_iter()
        .map(|path| repo_path(&scope, path))
        .collect();
    let format = args.format.unwrap_or(args::Format::Hunks);
    let response = ctx
        .call(ctx.client.get_diff_page(GetDiffRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            from_revision: args.from_revision.unwrap_or_default(),
            to_revision: args.to_revision.unwrap_or_default(),
            paths,
            format: v1::DiffFormat::from(format).into(),
            symbols: matches!(format, args::Format::Symbols),
            expand_symbols: args.expand.unwrap_or_default(),
            context_lines: args.context_lines,
            max_lines_per_file: args.max_lines_per_file.unwrap_or(0),
            at_operation: args.at_operation.unwrap_or_default(),
            skip_snapshot: args.skip_snapshot.unwrap_or(false),
            page_size: page(args.page_size, 20),
            page_token: args.page_token.unwrap_or_default(),
            ..GetDiffRequest::default()
        }))
        .await?;
    let diff = Diff::from(&response);
    to_value(&DiffPage {
        diff: brief::Diff::from(&diff),
        next_page_token: token(response.next_page_token),
    })
});

/// The operation log's filter, from a tool's inputs.
pub(super) fn filter(
    workspaces: Option<Vec<String>>,
    changes: Option<Vec<String>>,
    agents: Option<Vec<String>>,
    kinds: Option<Vec<args::Kind>>,
) -> Result<OperationFilter, ToolError> {
    Ok(OperationFilter {
        workspaces: workspaces
            .unwrap_or_default()
            .iter()
            .map(|ws| workspace_arg(ws))
            .collect::<Result<_, _>>()?,
        changes: changes.unwrap_or_default(),
        agents: agents.unwrap_or_default(),
        kinds: kinds
            .unwrap_or_default()
            .into_iter()
            .map(|kind| v1::OperationKind::from(kind).into())
            .collect(),
    })
}

tool!(list_operations, args::ListOperations, |call, args| {
    let ctx = call.ctx;
    let scope = call.scope(args.repo.as_ref(), None).await?;
    let filter = filter(args.workspaces, args.changes, args.agents, args.kinds)?;
    let response = ctx
        .call(ctx.client.list_operations_page(ListOperationsRequest {
            repo: scope.repo_id,
            filter: Some(filter),
            include_effects: args.include_effects.unwrap_or(false),
            page_size: page(args.page_size, 50),
            page_token: args.page_token.unwrap_or_default(),
            ..ListOperationsRequest::default()
        }))
        .await?;
    to_value(&Operations {
        operations: response.operations.iter().map(Operation::from).collect(),
        next_page_token: token(response.next_page_token),
    })
});

tool!(query_changes, args::QueryChanges, |call, args| {
    let ctx = call.ctx;
    let scope = call.scope(args.repo.as_ref(), None).await?;
    let since = match args.since {
        Some(since) => Some(humantime::parse_rfc3339_weak(&since).map_err(|_| {
            ToolError::invalid(
                "",
                format!("since {since:?} is not an RFC 3339 time"),
                "Pass since as an RFC 3339 time, such as 2026-10-01T00:00:00Z.",
            )
        })?),
        None => None,
    };
    let response = ctx
        .call(
            ctx.client.query_changes(QueryChangesRequest {
                repo: scope.repo_id.clone(),
                paths: args
                    .paths
                    .unwrap_or_default()
                    .into_iter()
                    .map(|path| repo_path(&scope, path))
                    .collect(),
                symbols: args.symbols.unwrap_or_default(),
                agents: args.agents.unwrap_or_default(),
                outcomes: args
                    .outcomes
                    .unwrap_or_default()
                    .into_iter()
                    .map(|outcome| {
                        i32::from(match outcome {
                            args::Outcome::Open => v1::ChangeOutcome::Open,
                            args::Outcome::Landed => v1::ChangeOutcome::Landed,
                            args::Outcome::Abandoned => v1::ChangeOutcome::Abandoned,
                            args::Outcome::Undone => v1::ChangeOutcome::Undone,
                        })
                    })
                    .collect(),
                since: since.map(prost_types::Timestamp::from),
                page_size: page(args.page_size, 20),
                page_token: args.page_token.unwrap_or_default(),
                ..QueryChangesRequest::default()
            }),
        )
        .await?;
    to_value(&HistoryPage {
        history: History::from(&response),
        next_page_token: token(response.next_page_token),
    })
});

tool!(get_operation, args::GetOperation, |call, args| {
    let ctx = call.ctx;
    let scope = call.scope(args.repo.as_ref(), None).await?;
    let response = ctx
        .call(ctx.client.get_operation(&scope.repo_id, &args.operation_id))
        .await?;
    let operation = response.operation.ok_or_else(|| missing("operation"))?;
    to_value(&Operation::from(&operation))
});

/// `read_file`'s default range.
const READ_DEFAULT: u64 = 64 << 10;
/// `read_file`'s largest range: `ReadFile`'s.
const READ_MAX: u64 = 1 << 20;

tool!(read_file, args::ReadFile, |call, args| {
    let ctx = call.ctx;
    let limit = args.limit.unwrap_or(READ_DEFAULT);
    if limit > READ_MAX {
        return Err(ToolError::invalid(
            "",
            format!("limit {limit} is over {READ_MAX}"),
            "Pass a limit of at most 1048576 bytes, and read on from next_offset.",
        ));
    }
    let scope = call
        .scope(args.repo.as_ref(), args.workspace.as_ref())
        .await?;
    let offset = args.offset.unwrap_or(0);
    let mut response = ctx
        .call(ctx.client.read_file(ReadFileRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision: at(args.revision),
            path: repo_path(&scope, args.path),
            at_operation: args.at_operation.unwrap_or_default(),
            skip_snapshot: args.skip_snapshot.unwrap_or(false),
            offset,
            limit: if limit == 0 { READ_DEFAULT } else { limit },
        }))
        .await?;
    let end = offset.saturating_add(len(&response.content));
    response
        .content
        .truncate(text_end(&response.content, end < response.size));
    let end = offset.saturating_add(len(&response.content));
    to_value(&FileRange {
        file: FileRead::from(&response),
        offset,
        next_offset: (end < response.size).then_some(end),
    })
});

fn len(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap_or(u64::MAX)
}

/// Where a range of a file is cut so it stays text: before a UTF-8
/// character the range's end split, when the range is otherwise UTF-8 and
/// the file goes on (`more`). Anything else is left whole, and goes out as
/// base64 if it is not UTF-8; so does a range shorter than one character.
fn text_end(bytes: &[u8], more: bool) -> usize {
    match std::str::from_utf8(bytes) {
        Err(error) if more && error.error_len().is_none() && error.valid_up_to() > 0 => {
            error.valid_up_to()
        }
        _ => bytes.len(),
    }
}

tool!(list_files, args::ListFiles, |call, args| {
    let ctx = call.ctx;
    let scope = call
        .scope(args.repo.as_ref(), args.workspace.as_ref())
        .await?;
    let path = args
        .path
        .map(|path| repo_path(&scope, path))
        .unwrap_or_default();
    let response = ctx
        .call(ctx.client.list_files_page(ListFilesRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision: at(args.revision),
            path,
            recursive: args.recursive.unwrap_or(false),
            at_operation: args.at_operation.unwrap_or_default(),
            skip_snapshot: args.skip_snapshot.unwrap_or(false),
            page_size: page(args.page_size, 200),
            page_token: args.page_token.unwrap_or_default(),
        }))
        .await?;
    to_value(&Files {
        entries: response.entries.iter().map(FileEntry::from).collect(),
        commit_id: response.commit_id,
        at_operation: response.at_operation,
        next_page_token: token(response.next_page_token),
    })
});

tool!(create_workspace, args::CreateWorkspace, |call, args| {
    use create_workspace_request::Start;
    let ctx = call.ctx;
    let start = match (args.change, args.new_on) {
        (Some(_), Some(_)) => return Err(both("change", "new_on")),
        (Some(change), None) => Some(Start::Change(change)),
        (None, Some(new_on)) => Some(Start::NewOn(new_on)),
        (None, None) => None,
    };
    let scope = call.scope(args.repo.as_ref(), None).await?;
    let response = ctx
        .call(
            ctx.client.create_workspace(CreateWorkspaceRequest {
                repo: scope.repo_id,
                start,
                owner: call.server.leases.owner.clone(),
                purpose: args.purpose.unwrap_or_default(),
                missing_stores: args
                    .missing_stores
                    .map_or(0, |missing| v1::MissingStores::from(missing).into()),
                ..CreateWorkspaceRequest::default()
            }),
        )
        .await?;
    if let Some(ws) = &response.workspace {
        call.server.leases.keep(ctx, ws);
    }
    to_value(&Created::from_response(&response).ok_or_else(|| missing("workspace"))?)
});

tool!(snapshot_workspace, args::SnapshotWorkspace, |call, args| {
    let ctx = call.ctx;
    let (scope, ws) = call.workspace(args.workspace.as_ref()).await?;
    let response = ctx
        .call(ctx.client.snapshot_workspace(
            &scope.repo_id,
            &ws.id,
            &args.description.unwrap_or_default(),
        ))
        .await?;
    let workspace = workspace_after(ctx, Some(&ws.id)).await;
    to_value(&Snapshotted::from_response(&response, workspace))
});

tool!(move_workspace, args::MoveWorkspace, |call, args| {
    use move_workspace_request::Target;
    let ctx = call.ctx;
    let target = match (args.change, args.new_on) {
        (Some(_), Some(_)) => return Err(both("change", "new_on")),
        (Some(change), None) => Some(Target::Change(change)),
        (None, Some(new_on)) => Some(Target::NewOn(new_on)),
        (None, None) => None,
    };
    let (scope, ws) = call.workspace(Some(&args.workspace)).await?;
    let response = ctx
        .call(
            ctx.client.move_workspace(MoveWorkspaceRequest {
                repo: scope.repo_id,
                workspace: ws.id,
                target,
                missing_stores: args
                    .missing_stores
                    .map_or(0, |missing| v1::MissingStores::from(missing).into()),
            }),
        )
        .await?;
    to_value(&Moved::from_response(&response).ok_or_else(|| missing("workspace"))?)
});

tool!(release_workspace, args::ReleaseWorkspace, |call, args| {
    let ctx = call.ctx;
    let (scope, ws) = call.workspace(Some(&args.workspace)).await?;
    let response = ctx
        .call(
            ctx.client
                .release_workspace(&scope.repo_id, &ws.id, true, false),
        )
        .await?;
    call.server.leases.end(&ws.id);
    to_value(&Released::from(&response))
});

tool!(describe_change, args::DescribeChange, |call, args| {
    let ctx = call.ctx;
    let scope = call
        .scope(args.repo.as_ref(), args.workspace.as_ref())
        .await?;
    let response = ctx
        .call(ctx.client.describe_change(DescribeChangeRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision: at(args.revision),
            description: args.description,
            expected_commit_id: args.expected_commit_id.unwrap_or_default(),
        }))
        .await?;
    let workspace = workspace_after(ctx, scope.workspace_ref()).await;
    to_value(&Described::from_response(&response, workspace))
});

tool!(abandon_change, args::AbandonChange, |call, args| {
    let ctx = call.ctx;
    let scope = call
        .scope(args.repo.as_ref(), args.workspace.as_ref())
        .await?;
    let response = ctx
        .call(ctx.client.abandon_change(AbandonChangeRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision: args.revision,
            expected_commit_id: args.expected_commit_id.unwrap_or_default(),
        }))
        .await?;
    let workspace = workspace_after(ctx, scope.workspace_ref()).await;
    to_value(&Abandoned::from_response(&response, workspace))
});

tool!(undo_operation, args::UndoOperation, |call, args| {
    let ctx = call.ctx;
    let scope = call
        .scope(args.repo.as_ref(), args.workspace.as_ref())
        .await?;
    let response = ctx
        .call(ctx.client.undo_operation(UndoOperationRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            operation_id: args.operation_id,
        }))
        .await?;
    let workspace = workspace_after(ctx, scope.workspace_ref()).await;
    to_value(&Undone::from_response(&response, workspace))
});

/// The most edits one `edit_files` takes.
const EDITS_MAX: usize = 1000;

tool!(edit_files, args::EditFiles, |call, args| {
    use edit_files_request::Target;
    let ctx = call.ctx;
    let target = match (args.revision, args.new_on) {
        (Some(_), Some(_)) => {
            return Err(invalid_edit(
                None,
                "revision and new_on were both given",
                "Give revision (`@` for the workspace's change) or new_on, not both.",
            ));
        }
        (Some(revision), None) => Target::Revision(revision),
        (None, Some(new_on)) => Target::NewOn(new_on),
        (None, None) => {
            return Err(invalid_edit(
                None,
                "neither revision nor new_on was given",
                "Give revision (`@` for the workspace's change) or new_on.",
            ));
        }
    };
    if args.edits.is_empty() || args.edits.len() > EDITS_MAX {
        return Err(invalid_edit(
            None,
            format!("{} edits were given", args.edits.len()),
            "Give 1 to 1000 edits; split more across calls.",
        ));
    }
    // Checked before the call, so nothing is snapshotted for a mistake.
    let edits = args
        .edits
        .into_iter()
        .enumerate()
        .map(|(index, edit)| file_edit(index, edit))
        .collect::<Result<Vec<_>, _>>()?;
    let scope = call
        .scope(args.repo.as_ref(), args.workspace.as_ref())
        .await?;
    let edits = edits
        .into_iter()
        .map(|edit| v1::FileEdit {
            path: repo_path(&scope, edit.path),
            action: match edit.action {
                Some(file_edit::Action::CopyFrom(source)) => {
                    Some(file_edit::Action::CopyFrom(repo_path(&scope, source)))
                }
                action => action,
            },
            ..edit
        })
        .collect();
    let response = ctx
        .call(ctx.client.edit_files(EditFilesRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            target: Some(target),
            expected_commit_id: args.expected_commit_id.unwrap_or_default(),
            edits,
            description: args.description,
        }))
        .await?;
    let workspace = workspace_after(ctx, scope.workspace_ref()).await;
    to_value(&Edited::from_response(&response, workspace))
});

/// An edit the daemon would refuse, refused first: reason `INVALID_EDIT`,
/// as the daemon gives it, with the edit's index.
fn invalid_edit(index: Option<usize>, message: impl Into<String>, hint: &str) -> ToolError {
    let mut object = ErrorObject {
        kind: "invalid_argument".to_owned(),
        reason: reef_api::error::INVALID_EDIT.to_owned(),
        message: message.into(),
        hint: hint.to_owned(),
        ..ErrorObject::default()
    };
    if let Some(index) = index {
        object
            .metadata
            .insert("edit_index".to_owned(), index.to_string());
    }
    ToolError::Own(Box::new(object))
}

/// One edit as the request takes it, its shape checked.
fn file_edit(index: usize, edit: args::Edit) -> Result<v1::FileEdit, ToolError> {
    use file_edit::Action;
    let refuse = |message: &str, hint: &str| {
        invalid_edit(Some(index), format!("edit {index}: {message}"), hint)
    };
    let mut actions = Vec::new();
    if let Some(content) = edit.content {
        actions.push(Action::Content(content.into_bytes()));
    }
    if let Some(encoded) = edit.content_base64 {
        // Models wrap long base64 in lines.
        let encoded: String = encoded.split_ascii_whitespace().collect();
        let bytes = STANDARD.decode(encoded.as_bytes()).map_err(|_| {
            refuse(
                "content_base64 is not standard padded base64",
                "Encode binary content as standard padded base64.",
            )
        })?;
        actions.push(Action::Content(bytes));
    }
    if let Some(replace) = edit.replace {
        if replace.old_text.is_empty() {
            return Err(refuse(
                "replace.old_text is empty",
                "Give the text to find.",
            ));
        }
        actions.push(Action::Replace(v1::TextReplace {
            old_text: replace.old_text,
            new_text: replace.new_text,
            all: replace.all.unwrap_or(false),
        }));
    }
    if edit.delete == Some(true) {
        actions.push(Action::Delete(v1::FileDelete {}));
    }
    if let Some(source) = edit.copy_from {
        actions.push(Action::CopyFrom(source));
    }
    if actions.len() > 1 {
        return Err(refuse(
            "more than one action",
            "Give one of content, content_base64, replace, delete and copy_from per edit; a rename is a copy_from edit and a delete edit.",
        ));
    }
    let action = actions.pop();
    if action.is_none() && edit.executable.is_none() {
        return Err(refuse(
            "no action",
            "Give one of content, content_base64, replace, delete and copy_from, or executable.",
        ));
    }
    Ok(v1::FileEdit {
        path: edit.path,
        action,
        executable: edit.executable,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use prost_reflect::DescriptorPool;
    use prost_reflect::prost_types::method_options::IdempotencyLevel;

    use super::*;

    /// R2.13 test 2: the annotations follow the schema, and every v1
    /// method is a tool or says why not.
    #[test]
    fn annotations_follow_the_schema() {
        let pool = DescriptorPool::decode(reef_api::FILE_DESCRIPTOR_SET).unwrap();
        let method = |path: &str| {
            let (service, name) = path.trim_start_matches('/').split_once('/').unwrap();
            pool.get_service_by_name(service)
                .and_then(|service| service.methods().find(|m| m.name() == name))
                .unwrap_or_else(|| panic!("{path} is not a v1 method"))
        };
        for tool in TOOLS {
            let access = reef_api::access(tool.rpc).unwrap();
            assert_ne!(access, v1::Access::Admin, "{}", tool.name);
            let annotations = tool.annotations();
            assert_eq!(
                annotations.read_only_hint,
                Some(access == v1::Access::Read),
                "{}",
                tool.name
            );
            assert_eq!(annotations.open_world_hint, Some(false), "{}", tool.name);
            if access == v1::Access::Write {
                let idempotent = method(tool.rpc)
                    .method_descriptor_proto()
                    .options
                    .as_ref()
                    .and_then(|options| options.idempotency_level)
                    == Some(IdempotencyLevel::Idempotent.into());
                assert_eq!(
                    annotations.idempotent_hint,
                    Some(idempotent),
                    "{}",
                    tool.name
                );
                assert!(annotations.destructive_hint.is_some(), "{}", tool.name);
            }
        }
        let tools: BTreeSet<&str> = TOOLS.iter().map(|tool| tool.rpc).collect();
        let not_tools: BTreeSet<&str> = NOT_TOOLS.iter().map(|(rpc, _)| *rpc).collect();
        assert!(
            tools.is_disjoint(&not_tools),
            "{:?}",
            tools.intersection(&not_tools)
        );
        for path in reef_api::methods() {
            assert!(
                tools.contains(path) || not_tools.contains(path),
                "{path} is neither in TOOLS nor in NOT_TOOLS"
            );
        }
        for path in &not_tools {
            method(path);
        }
        let names: BTreeSet<&str> = TOOLS.iter().map(|tool| tool.name).collect();
        assert_eq!(names.len(), 21);
    }

    #[test]
    fn schemas_are_objects_that_refuse_unknown_fields() {
        for tool in TOOLS {
            let schema = (tool.schema)();
            assert_eq!(schema["type"], "object", "{}", tool.name);
            assert_eq!(schema["additionalProperties"], false, "{}", tool.name);
        }
    }

    /// Review finding 1: compaction drops keywords, never a field, at any
    /// depth (`get_diff`'s `format` was taken for the integer keyword).
    #[test]
    fn compaction_keeps_every_field() {
        /// Every field's path, through `$ref`, `anyOf`, and `items`.
        fn fields(schema: &Value, root: &Value, at: &str, out: &mut Vec<String>) {
            if let Some(name) = schema["$ref"]
                .as_str()
                .and_then(|r| r.strip_prefix("#/$defs/"))
            {
                return fields(&root["$defs"][name], root, at, out);
            }
            if let Some(Value::Array(any)) = schema.get("anyOf") {
                for option in any {
                    fields(option, root, at, out);
                }
            }
            if let Some(items) = schema.get("items") {
                fields(items, root, &format!("{at}[]"), out);
            }
            if let Some(Value::Object(properties)) = schema.get("properties") {
                for (name, value) in properties {
                    out.push(format!("{at}/{name}"));
                    fields(value, root, &format!("{at}/{name}"), out);
                }
            }
        }
        fn same<T: JsonSchema + 'static>() {
            let collect = |schema: &Value| {
                let mut out = Vec::new();
                fields(schema, schema, "", &mut out);
                out.sort();
                out
            };
            let raw = Value::Object((*schema_for_input::<T>().unwrap()).clone());
            let compacted = Value::Object((*schema::<T>()).clone());
            assert_eq!(collect(&compacted), collect(&raw), "{compacted}");
        }
        same::<args::ListRepositories>();
        same::<args::ListWorkspaces>();
        same::<args::GetWorkspace>();
        same::<args::GetStatus>();
        same::<args::ListChanges>();
        same::<args::GetChange>();
        same::<args::GetDiff>();
        same::<args::ListOperations>();
        same::<args::GetOperation>();
        same::<args::ReadFile>();
        same::<args::ListFiles>();
        same::<args::WaitForOperations>();
        same::<args::CreateWorkspace>();
        same::<args::SnapshotWorkspace>();
        same::<args::MoveWorkspace>();
        same::<args::ReleaseWorkspace>();
        same::<args::DescribeChange>();
        same::<args::AbandonChange>();
        same::<args::UndoOperation>();
        same::<args::EditFiles>();
        let diff = schema::<args::GetDiff>();
        assert_eq!(
            diff["properties"]["format"]["enum"],
            serde_json::json!(["summary", "stat", "hunks", "symbols"]),
            "{diff:?}"
        );
        assert!(diff["properties"]["page_size"].get("format").is_none());
    }

    #[test]
    fn ranges_end_on_a_character() {
        let text = "aé".as_bytes();
        assert_eq!(text_end(&text[..2], true), 1);
        // At the end of the file, or not UTF-8 otherwise: whole.
        assert_eq!(text_end(&text[..2], false), 2);
        assert_eq!(text_end(&[0xff, b'a'], true), 2);
        // Shorter than one character: whole, as base64.
        assert_eq!(text_end(&text[1..2], true), 1);
        assert_eq!(text_end(text, true), 3);
    }

    #[test]
    fn edits_are_checked_before_the_call() {
        let edit = |value: Value| file_edit(3, serde_json::from_value(value).unwrap());
        let reason = |result: Result<v1::FileEdit, ToolError>| match result {
            Err(ToolError::Own(object)) => (object.reason, object.metadata["edit_index"].clone()),
            _ => panic!("not refused"),
        };
        for bad in [
            serde_json::json!({"path": "a", "content": "x", "delete": true}),
            serde_json::json!({"path": "a"}),
            serde_json::json!({"path": "a", "replace": {"old_text": "", "new_text": "x"}}),
            serde_json::json!({"path": "a", "content_base64": "not base64!"}),
        ] {
            assert_eq!(
                reason(edit(bad)),
                ("INVALID_EDIT".to_owned(), "3".to_owned())
            );
        }
        let chmod = edit(serde_json::json!({"path": "a", "executable": true}))
            .ok()
            .unwrap();
        assert_eq!((chmod.action, chmod.executable), (None, Some(true)));
        let bytes = edit(serde_json::json!({"path": "a", "content_base64": "AP8="}))
            .ok()
            .unwrap();
        assert_eq!(bytes.action, Some(file_edit::Action::Content(vec![0, 255])));
    }
}
