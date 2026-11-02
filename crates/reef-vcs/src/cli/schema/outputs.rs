//! What every leaf command prints, in each form ([`OUTPUTS`]): with `-o
//! json`, with `-o brief`, and on a pipe without `-o` (ADR 0045). The
//! schemas come from the types that print, so they cannot drift; a test
//! runs the commands and checks their output against them.
//!
//! Every PR that adds a command adds its row here; a test fails until it
//! does.

use reef_client::backup::Manifest;
use reef_client::brief;
use reef_client::json::{
    Abandoned, BuiltStore, ChangeSummary, Created, CreatedRemoteToken, Described, Diff, FileEntry,
    FileRead, GcReport, History, Inspected, LogEntry, Moved, Operation, Presence, PresenceEvent,
    Released, RemoteListener, RemoteToken, RepoStatus, Repository, Restored, Snapshotted, Status,
    Store, SymbolOccurrence, SymbolStatus, SymbolUpload, Undone, WebSession, Workspace,
};
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde_json::{Value, json};

use crate::cli::{guide, here, init, presence, remote, ws, ws_run};

/// A schema of what a command prints, built in the document's generator
/// so each type is defined once.
pub(crate) type Shape = fn(&mut SchemaGenerator) -> Schema;

/// What the brief form prints.
#[derive(Clone, Copy)]
pub(crate) enum Brief {
    /// The full JSON, compact.
    Same,
    /// A view of it (`reef_client::brief`).
    View(Shape),
}

/// What a pipe gets without `-o`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Piped {
    /// The brief form.
    Brief,
    /// The text form: prose, or another program's output.
    Text,
}

/// One leaf command's output.
pub(crate) struct OutputRow {
    pub(crate) command: &'static [&'static str],
    /// What `-o json` prints; `None` when stdout is never JSON.
    pub(crate) json: Option<Shape>,
    /// What `-o brief` prints.
    pub(crate) brief: Brief,
    /// What a pipe gets without `-o`.
    pub(crate) piped: Piped,
    /// One value per line, as each happens (`op watch`).
    pub(crate) lines: bool,
    /// The value ends stdout, after another program's output (`ws
    /// run`).
    pub(crate) last_value: bool,
    /// It exits non-zero and still prints its result: with another
    /// program's code (`ws run`, a failed `store build`), or when part of
    /// the work failed (`ws gc`); its other failures print the error
    /// document.
    pub(crate) result_on_failure: bool,
    /// Arguments (ids) that make a pipe get text unless `-o` asks for
    /// JSON (`init --print`).
    pub(crate) text_with: &'static [&'static str],
    /// Arguments (ids) that make stdout bytes whatever `-o` says (`schema
    /// --descriptor-set`).
    pub(crate) bytes_with: &'static [&'static str],
    /// What stdout carries when `json` is `None`, or a note on the output.
    pub(crate) note: Option<&'static str>,
}

/// One `T`.
fn object<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    generator.subschema_for::<T>()
}

/// An array of `T`.
fn list<T: JsonSchema>(generator: &mut SchemaGenerator) -> Schema {
    let items = generator.subschema_for::<T>();
    json_schema!({ "type": "array", "items": items })
}

/// `reef here`: inside a workspace or not, told apart by `in_workspace`.
fn here(generator: &mut SchemaGenerator) -> Schema {
    let inside = generator.subschema_for::<here::Inside>();
    let outside = generator.subschema_for::<here::Outside>();
    json_schema!({ "oneOf": [inside, outside] })
}

/// `reef init`: what it wrote, or with `--print` the section it would.
fn init(generator: &mut SchemaGenerator) -> Schema {
    let wrote = generator.subschema_for::<init::InitJson>();
    let print = generator.subschema_for::<init::PrintJson>();
    json_schema!({ "anyOf": [wrote, print] })
}

fn removed(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "object",
        "properties": { "removed": { "type": "string", "description": "The profile's name." } },
        "required": ["removed"],
    })
}

fn stopped(_: &mut SchemaGenerator) -> Schema {
    json_schema!({
        "type": "object",
        "properties": {
            "stopped": { "const": true },
            "pid": { "type": "integer", "minimum": 0, "description": "The daemon's process id." },
        },
        "required": ["stopped", "pid"],
    })
}

fn any_object(_: &mut SchemaGenerator) -> Schema {
    json_schema!({ "type": "object" })
}

/// A row whose brief form is its JSON, compact, and the pipe's default.
const fn row(command: &'static [&'static str], json: Shape) -> OutputRow {
    OutputRow {
        command,
        json: Some(json),
        brief: Brief::Same,
        piped: Piped::Brief,
        lines: false,
        last_value: false,
        result_on_failure: false,
        text_with: &[],
        bytes_with: &[],
        note: None,
    }
}

/// A row whose stdout is never JSON.
const fn none(command: &'static [&'static str], note: &'static str) -> OutputRow {
    OutputRow {
        command,
        json: None,
        brief: Brief::Same,
        piped: Piped::Text,
        lines: false,
        last_value: false,
        result_on_failure: false,
        text_with: &[],
        bytes_with: &[],
        note: Some(note),
    }
}

/// A row whose brief form is a view.
const fn view(command: &'static [&'static str], json: Shape, brief: Shape) -> OutputRow {
    OutputRow {
        brief: Brief::View(brief),
        ..row(command, json)
    }
}

/// A row that prints text on a pipe unless `-o` asks for JSON.
const fn text(command: &'static [&'static str], json: Shape, note: &'static str) -> OutputRow {
    OutputRow {
        piped: Piped::Text,
        note: Some(note),
        ..row(command, json)
    }
}

/// A stream: one value per line as each happens.
const fn lines(command: &'static [&'static str], json: Shape) -> OutputRow {
    OutputRow {
        lines: true,
        ..row(command, json)
    }
}

/// Every leaf command, in `--help`'s order.
pub(crate) const OUTPUTS: &[OutputRow] = &[
    row(&["ws", "create"], object::<Created>),
    view(
        &["ws", "list"],
        list::<Workspace>,
        list::<brief::Workspace<'static>>,
    ),
    row(&["ws", "inspect"], object::<Inspected>),
    row(&["ws", "snapshot"], object::<Snapshotted>),
    row(&["ws", "renew"], object::<ws::Renewed>),
    row(&["ws", "release"], object::<Released>),
    row(&["ws", "move"], object::<Moved>),
    OutputRow {
        last_value: true,
        result_on_failure: true,
        ..text(
            &["ws", "run"],
            object::<ws_run::Report>,
            "stdout is the command's own; with -o json or -o brief the report follows it, ending stdout",
        )
    },
    OutputRow {
        note: Some("exits 8 (io), with the reports, when the jj step of a repository's GC failed"),
        result_on_failure: true,
        ..row(&["ws", "gc"], list::<GcReport>)
    },
    row(&["here"], here),
    view(
        &["status"],
        object::<RepoStatus>,
        object::<brief::RepoStatus<'static>>,
    ),
    row(&["log"], list::<LogEntry>),
    row(&["show"], object::<ChangeSummary>),
    view(&["diff"], object::<Diff>, object::<brief::Diff<'static>>),
    row(&["describe"], object::<Described>),
    row(&["abandon"], object::<Abandoned>),
    row(&["undo"], object::<Undone>),
    row(&["op", "log"], list::<Operation>),
    row(&["op", "show"], object::<Operation>),
    lines(&["op", "watch"], object::<Operation>),
    row(&["history"], object::<History>),
    lines(&["presence", "watch"], object::<PresenceEvent>),
    row(&["presence", "report"], object::<Presence>),
    row(&["presence", "clear"], object::<presence::Cleared>),
    none(
        &["presence", "hook", "edit"],
        "nothing; reads a harness's hook event on stdin",
    ),
    none(
        &["presence", "hook", "end"],
        "nothing; reads a harness's hook event on stdin",
    ),
    row(&["ui"], object::<WebSession>),
    row(&["remote", "add"], object::<remote::Added>),
    row(&["remote", "list"], list::<remote::Listed>),
    row(&["remote", "remove"], removed),
    row(&["remote", "show"], object::<RemoteListener>),
    row(&["remote", "token", "create"], object::<CreatedRemoteToken>),
    row(&["remote", "token", "list"], list::<RemoteToken>),
    row(&["remote", "token", "rotate"], object::<CreatedRemoteToken>),
    row(&["remote", "token", "revoke"], object::<RemoteToken>),
    text(
        &["file", "read"],
        object::<FileRead>,
        "the file's bytes unless -o json or -o brief",
    ),
    row(&["file", "list"], list::<FileEntry>),
    OutputRow {
        note: Some(
            "with -o json or -o brief, only once the index is complete; otherwise the error",
        ),
        ..row(&["symbols", "index"], object::<SymbolStatus>)
    },
    row(&["symbols", "upload"], object::<SymbolUpload>),
    row(&["symbols", "list"], list::<SymbolOccurrence>),
    text(
        &["guide"],
        object::<guide::GuideJson>,
        "the guide as Markdown unless -o json or -o brief",
    ),
    OutputRow {
        note: Some("`--print` prints the section as text unless -o json or -o brief"),
        text_with: &["print"],
        ..row(&["init"], init)
    },
    none(&["mcp"], "the MCP protocol (JSON-RPC over stdio)"),
    row(&["repo", "register"], object::<Repository>),
    row(&["repo", "list"], list::<Repository>),
    row(&["repo", "forget"], object::<Repository>),
    OutputRow {
        note: Some(
            "the build's own output goes to stderr with -o json or -o brief; a failed build prints its result and exits with the build's code",
        ),
        result_on_failure: true,
        ..row(&["store", "build"], object::<BuiltStore>)
    },
    row(&["store", "list"], list::<Store>),
    none(
        &["daemon", "run"],
        "nothing; the daemon replaces this process and logs to stderr",
    ),
    row(&["daemon", "start"], object::<Status>),
    row(&["daemon", "stop"], stopped),
    row(&["daemon", "status"], object::<Status>),
    text(
        &["config", "show"],
        any_object,
        "TOML unless -o json or -o brief; the JSON is the daemon's configuration as `reef-daemon config show --json` prints it",
    ),
    row(&["backup"], object::<Manifest>),
    row(&["restore"], object::<Restored>),
    none(&["completions"], "a shell completion script"),
    OutputRow {
        note: Some(
            "this document; `--descriptor-set` prints the API's protobuf descriptor set instead",
        ),
        bytes_with: &["descriptor_set"],
        ..row(&["schema"], any_object)
    },
];

/// The row of the command at `words`.
pub(crate) fn row_of(words: &[String]) -> Option<&'static OutputRow> {
    OUTPUTS
        .iter()
        .find(|row| row.command.iter().eq(words.iter()))
}

/// A row as the document gives it: `{json, brief, piped, lines,
/// last_value, result_on_failure, text_with, bytes_with, note?}`,
/// each schema a reference into the document's definitions.
pub(crate) fn describe(row: &OutputRow, generator: &mut SchemaGenerator) -> Value {
    let json_schema = row.json.map(|shape| Value::from(shape(generator)));
    let brief = match (row.brief, &json_schema) {
        (Brief::View(shape), _) => Some(Value::from(shape(generator))),
        (Brief::Same, json_schema) => json_schema.clone(),
    };
    let mut entry = json!({
        "json": json_schema,
        "brief": brief,
        "piped": match row.piped {
            Piped::Brief => "brief",
            Piped::Text => "text",
        },
        "lines": row.lines,
        "last_value": row.last_value,
        "result_on_failure": row.result_on_failure,
        "text_with": row.text_with,
        "bytes_with": row.bytes_with,
    });
    if let Some(note) = row.note {
        entry["note"] = json!(note);
    }
    entry
}
