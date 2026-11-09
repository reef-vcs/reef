//! `reef conflict` (R3.25, ADR 0065): jj's stored conflicts as data.
//!
//! A conflict is one conflicted commit's: every term of jj's merge (sides
//! and bases) with jj's label and the commit it came from, each side's
//! provenance, and every conflicted path with each term's value and, with
//! `--hunks`, its hunks. A term's source is `verified` only when that
//! commit holds the term's content at the conflicted paths; a `label`
//! source is only what jj's label, which any client writes, says.
//!
//! `show --policy-input` prints the conflict as R3.07's `ConflictInput`,
//! for `reef policy check --input`: the seam R3.28 builds on.

use std::io::{self, Write};

use reef_api::v1::{self, GetConflictRequest, ListConflictsRequest};
use reef_client::json::{self, reverse_hex};
use reef_policy::input::{ConflictInput, ConflictSide};
use schemars::JsonSchema;
use serde::Serialize;

use super::advice::AdviceCtx;
use super::out::{TextRender, TextRow};
use super::scope::{self, ScopeArgs};
use super::{CliError, Ctx};

/// Ids in text, by their first characters.
const ID_WIDTH: usize = 12;
/// Paths a listing's text row names.
const ROW_PATHS: usize = 3;

/// `reef conflict`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: ConflictCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ConflictCommand {
    /// List conflicted changes: their sides, where each came from, and their paths.
    #[command(
        visible_alias = "ls",
        after_help = "Example: reef conflict list --introduced"
    )]
    List(ListArgs),
    /// Show one conflict: each side's source and provenance, and each file's terms and hunks.
    #[command(after_help = "Example: reef conflict show -r @ --hunks")]
    Show(ShowArgs),
}

/// `reef conflict list`.
#[derive(Debug, clap::Args)]
struct ListArgs {
    /// The commits to look at; `mutable() & conflicts()` when not given.
    #[arg(short = 'r', long = "revisions", value_name = "REVSET")]
    revisions: Option<String>,
    /// Leave out commits whose every conflicted path is inherited from a parent.
    #[arg(long)]
    introduced: bool,
    /// At most this many (0 for all).
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef conflict show`.
#[derive(Debug, clap::Args)]
struct ShowArgs {
    /// The conflicted revision [default: the workspace's change].
    #[arg(short = 'r', long = "revision", value_name = "REV")]
    revision: Option<String>,
    /// Only files equal to, or under, this repository-relative path (repeatable).
    #[arg(long = "path", value_name = "PATH")]
    paths: Vec<String>,
    /// Also split each file into resolved and conflicted hunks.
    #[arg(long)]
    hunks: bool,
    /// Print the conflict as a policy's conflict input, for `reef policy check --input`.
    #[arg(long, conflicts_with = "hunks")]
    policy_input: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// A conflict's kind: `tree` (jj's stored conflict); later tickets add
/// others.
pub(crate) fn kind_name(kind: i32) -> Option<&'static str> {
    match v1::ConflictKind::try_from(kind) {
        Ok(v1::ConflictKind::Tree) => Some("tree"),
        Ok(v1::ConflictKind::Unspecified) => None,
        Err(_) => Some(json::UNKNOWN),
    }
}

/// A term's role: `side` or `base`.
pub(crate) fn role_name(role: i32) -> Option<&'static str> {
    match v1::ConflictTermRole::try_from(role) {
        Ok(v1::ConflictTermRole::Side) => Some("side"),
        Ok(v1::ConflictTermRole::Base) => Some("base"),
        Ok(v1::ConflictTermRole::Unspecified) => None,
        Err(_) => Some(json::UNKNOWN),
    }
}

/// How a source was decided: `verified`, `label` or `none`.
pub(crate) fn attribution_name(attribution: i32) -> Option<&'static str> {
    match v1::ConflictAttribution::try_from(attribution) {
        Ok(v1::ConflictAttribution::Verified) => Some("verified"),
        Ok(v1::ConflictAttribution::Label) => Some("label"),
        Ok(v1::ConflictAttribution::None) => Some("none"),
        Ok(v1::ConflictAttribution::Unspecified) => None,
        Err(_) => Some(json::UNKNOWN),
    }
}

/// How a path conflicts.
pub(crate) fn file_kind_name(kind: i32) -> Option<&'static str> {
    match v1::ConflictFileKind::try_from(kind) {
        Ok(v1::ConflictFileKind::Content) => Some("content"),
        Ok(v1::ConflictFileKind::Executable) => Some("executable"),
        Ok(v1::ConflictFileKind::AddAdd) => Some("add-add"),
        Ok(v1::ConflictFileKind::ModifyDelete) => Some("modify-delete"),
        Ok(v1::ConflictFileKind::Type) => Some("type"),
        Ok(v1::ConflictFileKind::Other) => Some("other"),
        Ok(v1::ConflictFileKind::Unspecified) => None,
        Err(_) => Some(json::UNKNOWN),
    }
}

/// What a term holds at a path.
pub(crate) fn value_type_name(kind: i32) -> Option<&'static str> {
    match v1::ConflictValueType::try_from(kind) {
        Ok(v1::ConflictValueType::Absent) => Some("absent"),
        Ok(v1::ConflictValueType::File) => Some("file"),
        Ok(v1::ConflictValueType::Symlink) => Some("symlink"),
        Ok(v1::ConflictValueType::Tree) => Some("tree"),
        Ok(v1::ConflictValueType::Submodule) => Some("submodule"),
        Ok(v1::ConflictValueType::Unspecified) => None,
        Err(_) => Some(json::UNKNOWN),
    }
}

macro_rules! names_schema {
    ($($schema:ident = $name:ident;)*) => {$(
        fn $schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
            reef_client::schema::names($name, true)
        }
    )*};
}

names_schema! {
    kind_schema = kind_name;
    role_schema = role_name;
    attribution_schema = attribution_name;
    file_kind_schema = file_kind_name;
    value_type_schema = value_type_name;
}

fn text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn time(value: Option<&prost_types::Timestamp>) -> Option<String> {
    value.map(json::rfc3339)
}

/// The commit a term came from.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub(crate) struct ConflictSource {
    /// The commit; `null` when no source was found. It may be hidden.
    commit_id: Option<String>,
    /// Its change, in hex.
    change_id: Option<String>,
    /// `verified` when the commit holds the term's content at every
    /// checked conflicted path; `label` when only jj's label names it;
    /// `none`.
    #[schemars(schema_with = "attribution_schema")]
    attribution: Option<String>,
    /// Its full description.
    description: Option<String>,
    /// Its author.
    author: Option<json::Signature>,
    /// When it was committed.
    committed_at: Option<String>,
}

/// Who produced a side: provenance records of the side's change.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub(crate) struct ConflictSideProvenance {
    /// The change the records are of.
    change_id: String,
    /// The record of the source commit itself, when one exists.
    commit_record: Option<json::ProvenanceRecord>,
    /// The change's newest record at or before the source commit.
    latest_record: Option<json::ProvenanceRecord>,
    /// The sessions with records of the change, newest first, at most 10;
    /// `records` counts each one's records of this change.
    sessions: Vec<json::ProvenanceSession>,
    /// Sessions with records of the change, in all.
    sessions_total: u32,
    /// Records of the change, in all.
    records: u64,
    /// Of the newest 200, those that do not verify.
    invalid_records: u64,
}

/// One term of the conflicted tree.
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub(crate) struct ConflictTerm {
    /// Its index in jj's order: side 0, base 0, side 1, ….
    index: u32,
    /// `side` or `base`.
    #[schemars(schema_with = "role_schema")]
    role: Option<String>,
    /// jj's label, verbatim; free text any client can write.
    label: Option<String>,
    /// The label's last parenthesized suffix, as written.
    origin: Option<String>,
    /// The commit it came from.
    source: Option<ConflictSource>,
    /// Sides with a source: who produced it.
    provenance: Option<ConflictSideProvenance>,
}

/// One term's value at a path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct ConflictValue {
    /// The term's index.
    index: u32,
    /// What it holds.
    #[serde(rename = "type")]
    #[schemars(schema_with = "value_type_schema")]
    value_type: Option<String>,
    /// The object id; `null` when absent.
    id: Option<String>,
    /// The executable bit, for a file.
    executable: bool,
}

/// One hunk's text on one term.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct ConflictHunkTerm {
    /// The term's index.
    index: u32,
    /// Its text.
    text: String,
    /// Its first line in that term's file, from 1.
    start_line: u32,
    /// Its lines.
    line_count: u32,
}

/// One hunk of a conflicted file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct ConflictHunk {
    /// Every term agrees here.
    resolved: bool,
    /// The agreed text, for a resolved hunk.
    text: Option<String>,
    /// Each term's text, for a conflicted hunk.
    terms: Vec<ConflictHunkTerm>,
}

/// One conflicted path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct ConflictFile {
    /// The path, repository-relative.
    path: String,
    /// SHA-256 of the path and its simplified terms.
    id: String,
    /// How it conflicts.
    #[schemars(schema_with = "file_kind_schema")]
    kind: Option<String>,
    /// A parent carries the same conflict at this path.
    inherited: bool,
    /// Each term's value (`show` only).
    terms: Vec<ConflictValue>,
    /// The file as hunks (`show --hunks` only).
    hunks: Vec<ConflictHunk>,
    /// A term is binary: no hunks.
    binary: bool,
    /// A term is too large: no hunks.
    too_large: bool,
}

/// One conflicted commit's conflict (R3.25).
#[derive(Clone, Debug, PartialEq, Serialize, JsonSchema)]
pub(crate) struct Conflict {
    /// `cfl_` and 32 hex digits.
    id: String,
    /// `tree`: jj's stored conflict.
    #[schemars(schema_with = "kind_schema")]
    kind: Option<String>,
    /// The conflicted commit.
    change: Option<json::Change>,
    /// Its number of sides.
    arity: u32,
    /// Every term, in jj's order.
    terms: Vec<ConflictTerm>,
    /// Who produced the conflicted change itself.
    change_provenance: Option<ConflictSideProvenance>,
    /// Conflicted paths: all of them in `show`, the first 20 in `list`.
    files: Vec<ConflictFile>,
    /// Conflicted paths, in all.
    files_total: u32,
    /// Conflicted paths no parent carries.
    introduced: u32,
    /// More than 64 terms: terms and sources are not reported.
    too_wide: bool,
}

fn provenance_of(summary: &v1::ConflictSideProvenance) -> ConflictSideProvenance {
    ConflictSideProvenance {
        change_id: summary.change_id.clone(),
        commit_record: summary
            .commit_record
            .as_ref()
            .map(json::ProvenanceRecord::from),
        latest_record: summary
            .latest_record
            .as_ref()
            .map(json::ProvenanceRecord::from),
        sessions: summary
            .sessions
            .iter()
            .map(json::ProvenanceSession::from)
            .collect(),
        sessions_total: summary.sessions_total,
        records: summary.records,
        invalid_records: summary.invalid_records,
    }
}

impl From<&v1::Conflict> for Conflict {
    fn from(conflict: &v1::Conflict) -> Self {
        Self {
            id: conflict.id.clone(),
            kind: kind_name(conflict.kind).map(str::to_owned),
            change: conflict.change.as_ref().map(json::Change::from),
            arity: conflict.arity,
            terms: conflict
                .terms
                .iter()
                .map(|term| ConflictTerm {
                    index: term.index,
                    role: role_name(term.role).map(str::to_owned),
                    label: text(&term.label),
                    origin: text(&term.origin),
                    source: term.source.as_ref().map(|source| ConflictSource {
                        commit_id: text(&source.commit_id),
                        change_id: text(&source.change_id),
                        attribution: attribution_name(source.attribution).map(str::to_owned),
                        description: text(&source.description),
                        author: source.author.as_ref().map(json::Signature::from),
                        committed_at: time(source.committed_at.as_ref()),
                    }),
                    provenance: term.provenance.as_ref().map(provenance_of),
                })
                .collect(),
            change_provenance: conflict.change_provenance.as_ref().map(provenance_of),
            files: conflict
                .files
                .iter()
                .map(|file| ConflictFile {
                    path: file.path.clone(),
                    id: file.id.clone(),
                    kind: file_kind_name(file.kind).map(str::to_owned),
                    inherited: file.inherited,
                    terms: file
                        .terms
                        .iter()
                        .map(|value| ConflictValue {
                            index: value.index,
                            value_type: value_type_name(value.r#type).map(str::to_owned),
                            id: text(&value.id),
                            executable: value.executable,
                        })
                        .collect(),
                    hunks: file
                        .hunks
                        .iter()
                        .map(|hunk| ConflictHunk {
                            resolved: hunk.resolved,
                            text: hunk.resolved.then(|| hunk.text.clone()),
                            terms: hunk
                                .terms
                                .iter()
                                .map(|term| ConflictHunkTerm {
                                    index: term.index,
                                    text: term.text.clone(),
                                    start_line: term.start_line,
                                    line_count: term.line_count,
                                })
                                .collect(),
                        })
                        .collect(),
                    binary: file.binary,
                    too_large: file.too_large,
                })
                .collect(),
            files_total: conflict.files_total,
            introduced: conflict.introduced,
            too_wide: conflict.too_wide,
        }
    }
}

/// A side in brief: where it came from, how surely, and its newest agent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct BriefSide<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    origin: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    change_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    attribution: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent: Option<&'a str>,
}

/// A conflict in brief. Always `id`, `arity`, `files_total` and
/// `introduced`; `change_id` and `sides` (`origin`, `change_id`,
/// `attribution`, and the newest record's `agent`). Dropped: everything
/// else; `-o json` has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct BriefConflict<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    change_id: Option<&'a str>,
    arity: u32,
    files_total: u32,
    introduced: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sides: Vec<BriefSide<'a>>,
}

impl<'a> From<&'a Conflict> for BriefConflict<'a> {
    fn from(conflict: &'a Conflict) -> Self {
        Self {
            id: &conflict.id,
            change_id: conflict.change.as_ref().map(|c| c.change_id.as_str()),
            arity: conflict.arity,
            files_total: conflict.files_total,
            introduced: conflict.introduced,
            sides: conflict
                .terms
                .iter()
                .filter(|term| term.role.as_deref() == Some("side"))
                .map(|term| BriefSide {
                    origin: term.origin.as_deref(),
                    change_id: term.source.as_ref().and_then(|s| s.change_id.as_deref()),
                    attribution: term.source.as_ref().and_then(|s| s.attribution.as_deref()),
                    agent: agent_of(term),
                })
                .collect(),
        }
    }
}

/// The agent of a side's newest record.
fn agent_of(term: &ConflictTerm) -> Option<&str> {
    term.provenance
        .as_ref()
        .and_then(|p| p.latest_record.as_ref())
        .and_then(|record| record.agent.as_deref())
}

/// `text` with control characters escaped, for a terminal: labels,
/// descriptions and paths come from whoever wrote them (S-130).
pub(crate) fn clean(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        // Controls, and the bidirectional overrides and isolates that
        // reorder what a terminal shows.
        let bidi = matches!(c, '\u{200e}' | '\u{200f}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
        if c.is_control() || bidi {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

fn short(id: &str) -> String {
    id.chars().take(ID_WIDTH).collect()
}

fn short_change(change_id: &str) -> String {
    short(&reverse_hex(change_id))
}

/// One term's heading: `side 0  rebase destination`.
fn term_name(term: &ConflictTerm, terms: &[ConflictTerm]) -> String {
    let role = term.role.as_deref().unwrap_or("term");
    let ordinal = terms
        .iter()
        .filter(|t| t.role == term.role && t.index < term.index)
        .count();
    format!("{role} {ordinal}")
}

fn source_text(source: Option<&ConflictSource>) -> String {
    let Some(source) = source else {
        return "source unknown".to_owned();
    };
    match (&source.commit_id, &source.change_id) {
        (Some(commit), Some(change)) => format!(
            "change {}  commit {}  {}",
            short_change(change),
            short(commit),
            source.attribution.as_deref().unwrap_or("none")
        ),
        _ => "no source found".to_owned(),
    }
}

fn provenance_text(provenance: Option<&ConflictSideProvenance>) -> Option<String> {
    let provenance = provenance?;
    let Some(record) = provenance.latest_record.as_ref() else {
        return Some("no provenance records".to_owned());
    };
    let mut parts = Vec::new();
    if let Some(agent) = &record.agent {
        parts.push(format!("agent {}", clean(agent)));
    }
    if let Some(identity) = record
        .identity
        .as_ref()
        .and_then(|identity| identity.key.as_ref())
        .map(|key| key.identity.as_str())
        .filter(|identity| !identity.is_empty())
    {
        parts.push(format!("identity {}", clean(identity)));
    }
    let session = provenance
        .sessions
        .iter()
        .find(|session| session.session_id == record.session_id);
    match session {
        Some(session) if session.ended_at.is_some() => {
            parts.push(format!("session {} (ended)", record.session_id));
        }
        Some(_) => parts.push(format!("session {} (live)", record.session_id)),
        None => parts.push(format!("session {}", record.session_id)),
    }
    let context = &record.context;
    if let Some(task) = &context.task {
        parts.push(format!("task {}", clean(task)));
    }
    if let Some(model) = &context.model {
        parts.push(format!("model {}", clean(model)));
    }
    if let Some(hash) = &context.instruction_sha256 {
        parts.push(format!("instruction {}", short(hash)));
    }
    if provenance.invalid_records > 0 {
        parts.push(format!(
            "{} records do not verify",
            provenance.invalid_records
        ));
    }
    Some(parts.join("  "))
}

impl Conflict {
    /// The heading of the term at `index`: `side 1`.
    fn name_of(&self, index: u32) -> String {
        self.terms
            .iter()
            .find(|term| term.index == index)
            .map_or_else(|| format!("term {index}"), |t| term_name(t, &self.terms))
    }

    /// One file: its kind, each term's value, and its conflicted hunks.
    fn render_file(&self, file: &ConflictFile, w: &mut dyn Write) -> io::Result<()> {
        let inherited = if file.inherited { "  (inherited)" } else { "" };
        writeln!(
            w,
            "{}  {}{inherited}",
            clean(&file.path),
            file.kind.as_deref().unwrap_or("-")
        )?;
        for value in &file.terms {
            let name = self.name_of(value.index);
            writeln!(
                w,
                "  {name:<7} {}{}{}",
                value.value_type.as_deref().unwrap_or("-"),
                value
                    .id
                    .as_deref()
                    .map(|id| format!(" {}", short(id)))
                    .unwrap_or_default(),
                if value.executable {
                    " (executable)"
                } else {
                    ""
                }
            )?;
        }
        if file.binary {
            writeln!(w, "  (binary: no hunks)")?;
        }
        if file.too_large {
            writeln!(w, "  (too large: no hunks)")?;
        }
        for hunk in file.hunks.iter().filter(|hunk| !hunk.resolved) {
            for term in &hunk.terms {
                let name = self.name_of(term.index);
                let origin = self
                    .terms
                    .iter()
                    .find(|t| t.index == term.index)
                    .and_then(|t| t.origin.as_deref())
                    .map(|o| format!("{}, ", clean(o)))
                    .unwrap_or_default();
                let last = term.start_line + term.line_count.saturating_sub(1);
                writeln!(w, "  --- {name} ({origin}lines {}-{last})", term.start_line)?;
                for line in term.text.lines() {
                    writeln!(w, "  {}", clean(line))?;
                }
            }
            writeln!(w, "  ---")?;
        }
        Ok(())
    }
}

impl TextRender for Conflict {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let change = self.change.as_ref();
        writeln!(
            w,
            "conflict {}  change {}  commit {}  {} sides  {} files ({} introduced)",
            self.id,
            change.map_or_else(String::new, |c| short_change(&c.change_id)),
            change.map_or_else(String::new, |c| short(&c.commit_id)),
            self.arity,
            self.files_total,
            self.introduced
        )?;
        if let Some(subject) = change.and_then(|c| c.description.lines().next()) {
            writeln!(w, "  {}", clean(subject))?;
        }
        if self.too_wide {
            writeln!(w, "more than 64 terms: sources are not reported")?;
        }
        for term in &self.terms {
            writeln!(
                w,
                "{:<7} {}  {}",
                term_name(term, &self.terms),
                clean(term.origin.as_deref().unwrap_or("-")),
                source_text(term.source.as_ref())
            )?;
            if let Some(line) = provenance_text(term.provenance.as_ref()) {
                writeln!(w, "        {line}")?;
            }
        }
        if let Some(line) = provenance_text(self.change_provenance.as_ref()) {
            writeln!(w, "owner   {line}")?;
        }
        for file in &self.files {
            self.render_file(file, w)?;
        }
        if (self.files.len() as u64) < u64::from(self.files_total) {
            writeln!(
                w,
                "({} of {} conflicted files shown)",
                self.files.len(),
                self.files_total
            )?;
        }
        writeln!(
            w,
            "`verified`: the commit holds that side's content at the conflicted paths; `label`: only jj's label says so"
        )
    }
}

impl TextRow for Conflict {
    const HEADER: &'static [&'static str] = &["CHANGE", "CONFLICT", "SIDES", "FILES", "PATHS"];

    fn row(&self) -> Vec<String> {
        let sides: Vec<String> = self
            .terms
            .iter()
            .filter(|term| term.role.as_deref() == Some("side"))
            .map(|term| {
                let attribution = term
                    .source
                    .as_ref()
                    .and_then(|s| s.attribution.as_deref())
                    .unwrap_or("none");
                match agent_of(term) {
                    Some(agent) => format!("{} ({attribution})", clean(agent)),
                    None => attribution.to_owned(),
                }
            })
            .collect();
        let mut paths: Vec<String> = self
            .files
            .iter()
            .take(ROW_PATHS)
            .map(|file| clean(&file.path))
            .collect();
        if self.files_total as usize > paths.len() {
            paths.push("…".to_owned());
        }
        vec![
            self.change
                .as_ref()
                .map_or_else(String::new, |c| short_change(&c.change_id)),
            short(&self.id),
            sides.join(", "),
            format!("{} ({} new)", self.files_total, self.introduced),
            paths.join(" "),
        ]
    }
}

/// What `show --policy-input` prints: an input `reef policy check --input`
/// reads.
#[derive(Clone, Debug, PartialEq, Serialize)]
struct PolicyInput {
    conflict: ConflictInput,
}

impl TextRender for PolicyInput {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let text = serde_json::to_string_pretty(self).map_err(io::Error::other)?;
        writeln!(w, "{text}")
    }
}

/// R3.07's conflict input from `conflict`: its change, every conflicted
/// path, and per side its source's change and the identity and agent of
/// the side's newest provenance record that verifies, for a `verified`
/// source only (empty otherwise). R3.28 extends this mapping.
pub(crate) fn policy_input(conflict: &v1::Conflict) -> ConflictInput {
    ConflictInput {
        change_id: conflict
            .change
            .as_ref()
            .map(|c| c.change_id.clone())
            .unwrap_or_default(),
        paths: conflict.files.iter().map(|f| f.path.clone()).collect(),
        sides: conflict
            .terms
            .iter()
            .filter(|term| term.role() == v1::ConflictTermRole::Side)
            .map(|term| {
                // Only a verified side speaks: a label is anyone's (S-127).
                let verified = term
                    .source
                    .as_ref()
                    .filter(|s| s.attribution() == v1::ConflictAttribution::Verified);
                let latest = verified
                    .and(term.provenance.as_ref())
                    .and_then(|p| p.latest_record.as_ref());
                ConflictSide {
                    change_id: verified.map(|s| s.change_id.clone()).unwrap_or_default(),
                    identity: latest
                        .and_then(|r| r.identity.as_ref())
                        .and_then(|i| i.key.as_ref())
                        .map(|k| k.identity.clone())
                        .unwrap_or_default(),
                    agent: latest.map(|r| r.agent.clone()).unwrap_or_default(),
                }
            })
            .collect(),
    }
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        ConflictCommand::List(args) => Box::pin(list(ctx, args)).await,
        ConflictCommand::Show(args) => Box::pin(show(ctx, args)).await,
    }
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let found = ctx
        .call(Box::pin(ctx.client.list_conflicts(
            ListConflictsRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                revset: args.revisions.unwrap_or_default(),
                introduced_only: args.introduced,
                ..ListConflictsRequest::default()
            },
            args.limit,
        )))
        .await
        .map_err(|error| advice.refine(error))?;
    if found.more {
        ctx.out.note(&format!(
            "reef: more conflicts than --limit {}; raise it, or 0 for all",
            args.limit
        ));
    }
    let listed: Vec<Conflict> = found.items.iter().map(Conflict::from).collect();
    ctx.out.list_with(&listed, |listed| {
        listed.iter().map(BriefConflict::from).collect::<Vec<_>>()
    })
}

async fn show(ctx: &Ctx, args: ShowArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let revision = args.revision.unwrap_or_else(|| "@".to_owned());
    let found = ctx
        .call(Box::pin(ctx.client.get_conflict(GetConflictRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision,
            paths: args.paths,
            hunks: args.hunks,
            ..GetConflictRequest::default()
        })))
        .await
        .map_err(|error| advice.refine(error))?;
    let conflict = found.conflict.unwrap_or_default();
    if args.policy_input {
        let input = PolicyInput {
            conflict: policy_input(&conflict),
        };
        return ctx.out.object_with(&input, |input| input);
    }
    let shown = Conflict::from(&conflict);
    ctx.out.object_with(&shown, BriefConflict::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_characters_are_escaped() {
        assert_eq!(clean("a\u{1b}[31mred"), "a\\u{1b}[31mred");
        assert_eq!(clean("tab\there"), "tab\\there");
        assert_eq!(clean("plain é"), "plain é");
        assert_eq!(clean("a\u{202e}b"), "a\\u{202e}b");
    }

    #[test]
    fn names_cover_every_value() {
        for n in 1..=1 {
            assert!(kind_name(n).is_some_and(|name| name != json::UNKNOWN));
        }
        for n in 1..=2 {
            assert!(role_name(n).is_some_and(|name| name != json::UNKNOWN));
        }
        for n in 1..=3 {
            assert!(attribution_name(n).is_some_and(|name| name != json::UNKNOWN));
        }
        for n in 1..=6 {
            assert!(file_kind_name(n).is_some_and(|name| name != json::UNKNOWN));
        }
        for n in 1..=5 {
            assert!(value_type_name(n).is_some_and(|name| name != json::UNKNOWN));
        }
    }

    #[test]
    fn policy_input_takes_sides_and_paths() {
        let record = v1::ProvenanceRecord {
            agent: "bot".into(),
            identity: Some(v1::KeyIdentity {
                key: Some(v1::SessionKey {
                    identity: "alice".into(),
                    ..v1::SessionKey::default()
                }),
                ..v1::KeyIdentity::default()
            }),
            ..v1::ProvenanceRecord::default()
        };
        let side = |index: u32,
                    change: &str,
                    attribution: v1::ConflictAttribution,
                    record: Option<v1::ProvenanceRecord>| v1::ConflictTerm {
            index,
            role: v1::ConflictTermRole::Side.into(),
            source: Some(v1::ConflictSource {
                change_id: change.into(),
                attribution: attribution.into(),
                ..v1::ConflictSource::default()
            }),
            provenance: record.map(|record| v1::ConflictSideProvenance {
                latest_record: Some(record),
                ..v1::ConflictSideProvenance::default()
            }),
            ..v1::ConflictTerm::default()
        };
        let conflict = v1::Conflict {
            change: Some(v1::Change {
                change_id: "c".into(),
                ..v1::Change::default()
            }),
            terms: vec![
                side(
                    0,
                    "a",
                    v1::ConflictAttribution::Verified,
                    Some(record.clone()),
                ),
                v1::ConflictTerm {
                    index: 1,
                    role: v1::ConflictTermRole::Base.into(),
                    ..v1::ConflictTerm::default()
                },
                side(2, "b", v1::ConflictAttribution::Label, Some(record)),
            ],
            files: vec![v1::ConflictFile {
                path: "docs/a.md".into(),
                ..v1::ConflictFile::default()
            }],
            ..v1::Conflict::default()
        };
        let input = policy_input(&conflict);
        assert_eq!(input.change_id, "c");
        assert_eq!(input.paths, ["docs/a.md"]);
        assert_eq!(input.sides.len(), 2);
        assert_eq!(
            (
                input.sides[0].change_id.as_str(),
                input.sides[0].identity.as_str(),
                input.sides[0].agent.as_str()
            ),
            ("a", "alice", "bot")
        );
        // A side only its label names says nothing.
        assert_eq!(
            (
                input.sides[1].change_id.as_str(),
                input.sides[1].identity.as_str(),
                input.sides[1].agent.as_str()
            ),
            ("", "", "")
        );
    }
}
