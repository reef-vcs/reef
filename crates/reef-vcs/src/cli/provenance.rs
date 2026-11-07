//! `reef provenance` (R3.04, ADR 0058): the signed record the daemon keeps
//! of each commit it records for a workspace, and the context a client
//! declares for the workspace's session.
//!
//! A record's commit, change, workspace, session and times are what the
//! daemon saw; its model, harness, task, instruction hash and client
//! session are what a client declared, which Reef never verifies. An
//! instruction is sent as the SHA-256 of its exact bytes, computed here;
//! its text never leaves this process.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::SystemTime;

use reef_api::v1::{
    self, DeclareProvenanceRequest, GetProvenanceRequest, ListProvenanceRequest,
    ProvenanceDeclaration, get_provenance_request,
};
use reef_client::brief;
use reef_client::json::{ProvenanceDeclared, ProvenanceRecord, ProvenanceSession};
use serde::Serialize;

use super::advice::AdviceCtx;
use super::out::{TextRender, TextRow};
use super::scope::{self, ScopeArgs};
use super::{CliError, Ctx};

/// The environment variable `--model` defaults to.
pub(crate) const MODEL_ENV: &str = "REEF_MODEL";
/// The environment variable `--task` defaults to.
pub(crate) const TASK_ENV: &str = "REEF_TASK";

/// Ids in text, by their first characters.
const ID_WIDTH: usize = 12;

/// `reef provenance`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: ProvenanceCommand,
}

#[derive(Debug, clap::Subcommand)]
enum ProvenanceCommand {
    /// Show a commit's provenance record: who recorded it, and the declared context.
    #[command(
        after_help = "Example: reef provenance show @-   # --envelope prints the signed record"
    )]
    Show(ShowArgs),
    /// List provenance records, newest first: of changes, a workspace, or a session.
    #[command(
        visible_alias = "ls",
        after_help = "Example: reef provenance list --change @"
    )]
    List(ListArgs),
    /// Declare the model, task or instruction a workspace's work is under; snapshots first.
    #[command(
        after_help = "Example: reef provenance declare --instruction-file prompt.txt --task T-1"
    )]
    Declare(DeclareArgs),
}

/// `reef provenance show`.
#[derive(Debug, clap::Args)]
struct ShowArgs {
    /// The revision; its commit's record [default: the workspace's change].
    #[arg(value_name = "REV")]
    revision: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
    /// Also print the signed envelope and its statement.
    #[arg(long)]
    envelope: bool,
}

/// `reef provenance list`.
#[derive(Debug, clap::Args)]
struct ListArgs {
    /// Only records of this revision's change (repeatable).
    #[arg(long = "change", value_name = "REV")]
    changes: Vec<String>,
    /// Only records of this session (`ses_...`).
    #[arg(long, value_name = "ID")]
    session: Option<String>,
    /// Only records of the scope's workspace.
    #[arg(long)]
    mine: bool,
    /// Only records recorded since then: an RFC 3339 time, or a duration ago (2h).
    #[arg(long, value_name = "WHEN", value_parser = super::history::when)]
    since: Option<SystemTime>,
    /// At most this many (0 for all).
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef provenance declare`.
#[derive(Debug, clap::Args)]
struct DeclareArgs {
    #[command(flatten)]
    declare: DeclareFlags,
    /// The agent runtime and its version, as name/version.
    #[arg(long, value_name = "NAME/VERSION")]
    harness: Option<String>,
    /// The harness's own session id.
    #[arg(long, value_name = "ID")]
    client_session: Option<String>,
    /// The instruction's SHA-256, if already computed.
    #[arg(long, value_name = "HEX", conflicts_with_all = ["instruction_file", "no_instruction"])]
    instruction_sha256: Option<String>,
    /// Clear the instruction.
    #[arg(long, conflicts_with = "instruction_file")]
    no_instruction: bool,
    /// Do not snapshot first (only right after a snapshot).
    #[arg(long)]
    no_snapshot: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `--model`, `--task` and `--instruction-file`: what `ws create`, `ws
/// run` and `provenance declare` declare.
#[derive(Clone, Debug, Default, clap::Args)]
pub(crate) struct DeclareFlags {
    /// The model and its version, as the client names it ("" clears it).
    #[arg(long, value_name = "MODEL")]
    pub(crate) model: Option<String>,
    /// The task this work is for: a URI or a reference ("" clears it).
    #[arg(long, value_name = "REF")]
    pub(crate) task: Option<String>,
    /// A file (- for stdin) holding the instruction; only its SHA-256 is sent, of its exact bytes.
    #[arg(long, value_name = "PATH")]
    pub(crate) instruction_file: Option<PathBuf>,
}

/// The provenance flags of `ws create` and `ws run`: [`DeclareFlags`]'
/// three, and `--identity` and `--actor`, fixed for the workspace's
/// provenance session. Hidden from their `--help`, which stays within its
/// budget; its after-help names them, and `reef provenance declare --help`
/// describes the first three.
#[derive(Clone, Debug, Default, clap::Args)]
pub(crate) struct SessionFlags {
    /// The model and its version [default: `REEF_MODEL`, when valid].
    #[arg(long, value_name = "MODEL", hide = true)]
    pub(crate) model: Option<String>,
    /// The task this work is for [default: `REEF_TASK`, when valid].
    #[arg(long, value_name = "REF", hide = true)]
    pub(crate) task: Option<String>,
    /// A file (- for stdin) holding the instruction; only its SHA-256 is sent.
    #[arg(long, value_name = "PATH", hide = true)]
    pub(crate) instruction_file: Option<PathBuf>,
    /// The person identity its provenance is bound to [default: the daemon's default].
    #[arg(long, value_name = "NAME", hide = true)]
    pub(crate) identity: Option<String>,
    /// Who works in it: agent, executor or human (declared, not verified).
    #[arg(long, value_parser = ["agent", "executor", "human"], hide = true)]
    pub(crate) actor: Option<String>,
}

/// The SHA-256 of the file at `path` (`-` for stdin), read as it is, in
/// 64 KiB pieces: nothing is trimmed and nothing is kept.
fn instruction_digest(path: &PathBuf) -> Result<String, CliError> {
    let read = |result: io::Result<String>| {
        result.map_err(|error| {
            CliError::Usage(format!("--instruction-file {}: {error}", path.display()))
        })
    };
    if path.as_os_str() == "-" {
        return read(reef_client::evidence::sha256_hex_of(io::stdin().lock()));
    }
    let file = std::fs::File::open(path);
    read(file.and_then(reef_client::evidence::sha256_hex_of))
}

impl DeclareFlags {
    /// The declaration's context fields: `None` for each flag not given,
    /// and an empty value, which clears the field, for one given empty.
    fn declaration(&self) -> Result<ProvenanceDeclaration, CliError> {
        let clean = |value: &Option<String>| value.as_ref().map(|text| text.trim().to_owned());
        Ok(ProvenanceDeclaration {
            model: clean(&self.model),
            task: clean(&self.task),
            instruction_sha256: self
                .instruction_file
                .as_ref()
                .map(instruction_digest)
                .transpose()?,
            ..ProvenanceDeclaration::default()
        })
    }
}

/// `name`'s value, trimmed, when it is set, not empty, and passes `check`:
/// an environment default is a convenience, so one the daemon would refuse
/// declares nothing rather than failing the command.
fn env_value(name: &str, check: fn(&str) -> Option<String>) -> Option<String> {
    let value = std::env::var(name).ok()?;
    let value = value.trim();
    (!value.is_empty() && check(value).is_none()).then(|| value.to_owned())
}

impl SessionFlags {
    /// The model: `--model`, else a valid `REEF_MODEL`.
    fn model(&self) -> Option<String> {
        self.model
            .clone()
            .or_else(|| env_value(MODEL_ENV, reef_client::evidence::provenance::label_problem))
    }

    /// The task: `--task`, else a valid `REEF_TASK`.
    fn task(&self) -> Option<String> {
        self.task
            .clone()
            .or_else(|| env_value(TASK_ENV, reef_client::evidence::provenance::task_problem))
    }

    /// Whether every provenance field comes from the environment, none from
    /// a flag: a declaration a daemon with `[provenance] enabled = false`
    /// refuses is then dropped, since naming nothing is never refused.
    pub(crate) fn only_from_env(&self) -> bool {
        self.model.is_none()
            && self.task.is_none()
            && self.instruction_file.is_none()
            && self.identity.is_none()
            && self.actor.is_none()
    }

    /// `CreateWorkspaceRequest.provenance`; `None` when nothing is given.
    pub(crate) fn declaration(&self) -> Result<Option<ProvenanceDeclaration>, CliError> {
        let mut declaration = DeclareFlags {
            model: self.model(),
            task: self.task(),
            instruction_file: self.instruction_file.clone(),
        }
        .declaration()?;
        // A new workspace has nothing to clear: an empty value (an empty
        // `REEF_MODEL`, say) declares nothing.
        declaration.model = declaration.model.filter(|text| !text.is_empty());
        declaration.task = declaration.task.filter(|text| !text.is_empty());
        declaration.identity = self.identity.clone().unwrap_or_default();
        declaration.actor = match self.actor.as_deref() {
            Some("human") => v1::SessionActor::Human,
            Some("executor") => v1::SessionActor::Executor,
            Some(_) => v1::SessionActor::Agent,
            None => v1::SessionActor::Unspecified,
        }
        .into();
        Ok((declaration != ProvenanceDeclaration::default()).then_some(declaration))
    }

    /// `REEF_MODEL` and `REEF_TASK` for `ws run`'s command: what it
    /// declared, so a runtime inside can read them.
    pub(crate) fn env(&self) -> Vec<(&'static str, String)> {
        [(MODEL_ENV, self.model()), (TASK_ENV, self.task())]
            .into_iter()
            .filter_map(|(name, value)| value.map(|value| (name, value)))
            .collect()
    }
}

/// Runs `reef provenance`.
pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        ProvenanceCommand::Show(args) => Box::pin(show(ctx, args)).await,
        ProvenanceCommand::List(args) => Box::pin(list(ctx, args)).await,
        ProvenanceCommand::Declare(args) => declare(ctx, args).await,
    }
}

async fn show(ctx: &Ctx, args: ShowArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let revision = args.revision.unwrap_or_else(|| "@".to_owned());
    let found = ctx
        .call(ctx.client.get_provenance(GetProvenanceRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            target: Some(get_provenance_request::Target::Revision(revision.clone())),
        }))
        .await;
    let (record, latest_of) = match found {
        Ok(response) => (response.record.unwrap_or_default(), None),
        Err(CliError::Daemon(error)) if error.reason() == reef_api::error::PROVENANCE_NOT_FOUND => {
            // The commit has none of its own (a describe, a rebase): the
            // change's newest record instead.
            let change = error
                .metadata()
                .get("change_id")
                .cloned()
                .unwrap_or_default();
            if change.is_empty() {
                return Err(advice.refine(CliError::Daemon(error)));
            }
            let latest = ctx
                .call(ctx.client.list_provenance_page(ListProvenanceRequest {
                    repo: scope.repo_id.clone(),
                    change_ids: vec![change.clone()],
                    page_size: 1,
                    ..ListProvenanceRequest::default()
                }))
                .await?;
            match latest.records.into_iter().next() {
                Some(record) => (record, Some(change)),
                None => return Err(advice.refine(CliError::Daemon(error))),
            }
        }
        Err(error) => return Err(advice.refine(error)),
    };
    let mut shown = ProvenanceRecord::from(&record);
    if !args.envelope {
        shown.statement = None;
        shown.envelope = None;
    }
    ctx.out.object_with(
        &Shown {
            record: shown,
            latest_of,
        },
        |shown| brief::ProvenanceRecord::from(&shown.record),
    )
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let mut change_ids = Vec::with_capacity(args.changes.len());
    for revision in &args.changes {
        let change = ctx
            .call(ctx.client.get_change(v1::GetChangeRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                revision: revision.clone(),
                ..v1::GetChangeRequest::default()
            }))
            .await
            .map_err(|error| advice.refine(error))?;
        change_ids.push(change.change.unwrap_or_default().change_id);
    }
    let workspace_id = if args.mine {
        scope.workspace_id()
    } else {
        String::new()
    };
    let records = ctx
        .call(ctx.client.list_provenance(
            ListProvenanceRequest {
                repo: scope.repo_id.clone(),
                change_ids,
                workspace_id,
                session_id: args.session.unwrap_or_default(),
                since: args.since.map(prost_types::Timestamp::from),
                ..ListProvenanceRequest::default()
            },
            args.limit,
        ))
        .await
        .map_err(|error| advice.refine(error))?;
    let listed: Vec<ProvenanceRecord> = records.items.iter().map(ProvenanceRecord::from).collect();
    if records.more {
        ctx.out.note(&format!(
            "reef: more records than --limit {}; raise it, or 0 for all",
            args.limit
        ));
    }
    ctx.out.list_with(&listed, |listed| {
        listed
            .iter()
            .map(brief::ProvenanceRecord::from)
            .collect::<Vec<_>>()
    })
}

async fn declare(ctx: &Ctx, args: DeclareArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let Some(workspace) = scope.workspace.clone() else {
        return Err(scope::scope_required());
    };
    let advice = AdviceCtx::new(&scope);
    let mut declaration = args.declare.declaration()?;
    declaration.harness = args.harness.map(|text| text.trim().to_owned());
    declaration.client_session = args.client_session.map(|text| text.trim().to_owned());
    if let Some(sha256) = args.instruction_sha256 {
        declaration.instruction_sha256 = Some(sha256.trim().to_ascii_lowercase());
    }
    if args.no_instruction {
        declaration.instruction_sha256 = Some(String::new());
    }
    let response = ctx
        .call(ctx.client.declare_provenance(DeclareProvenanceRequest {
            repo: scope.repo_id.clone(),
            workspace: workspace.id.clone(),
            declaration: Some(declaration.clone()),
            no_snapshot: args.no_snapshot,
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let declared = ProvenanceDeclared {
        session: ProvenanceSession::from(&response.session.unwrap_or_default()),
        snapshot_op: response.operation.map(|operation| operation.id),
    };
    ctx.out.object(&Declared {
        result: declared,
        workspace: workspace.id,
        fields: changed_fields(&declaration),
    })
}

/// The fields a declaration sets or clears, as the CLI names them.
fn changed_fields(declaration: &ProvenanceDeclaration) -> Vec<&'static str> {
    [
        ("model", &declaration.model),
        ("harness", &declaration.harness),
        ("task", &declaration.task),
        ("instruction", &declaration.instruction_sha256),
        ("client session", &declaration.client_session),
    ]
    .into_iter()
    .filter(|(_, value)| value.is_some())
    .map(|(name, _)| name)
    .collect()
}

fn short(id: &str) -> &str {
    id.get(..ID_WIDTH).unwrap_or(id)
}

/// The record's identity, as a line: `alice (person) as agent "builder-3",
/// key held by the daemon, presence none, certified by 3fa9…`.
fn identity_text(record: &ProvenanceRecord) -> String {
    let Some(identity) = &record.identity else {
        return "unknown".to_owned();
    };
    let status = identity.status.as_deref().unwrap_or("unknown");
    let Some(key) = &identity.key else {
        return match status {
            "invalid" => "invalid: nothing vouches for the key".to_owned(),
            _ => format!("unbound key {}", short(&record.key_id)),
        };
    };
    let mut text = format!(
        "{} ({})",
        key.identity,
        key.identity_kind.as_deref().unwrap_or("-")
    );
    if let Some(actor) = &key.actor {
        let _ = write!(text, " as {actor}");
    }
    if let Some(agent) = &key.agent {
        let _ = write!(text, " {agent:?}");
    }
    let custody = match key.custody.as_deref() {
        Some("daemon") => "key held by the daemon",
        Some("client") => "key held by a client",
        _ => "key custody unknown",
    };
    let _ = write!(
        text,
        ", {custody}, presence {}, certified by {}",
        key.presence.as_deref().unwrap_or("-"),
        short(&key.authority_key_id)
    );
    if status != "bound" {
        text = format!("{status}: {text}");
    }
    text
}

/// The declared context, as one line; `none` when nothing is declared.
fn context_text(record: &ProvenanceRecord) -> String {
    let context = &record.context;
    let parts: Vec<String> = [
        ("model", context.model.as_deref()),
        ("harness", context.harness.as_deref()),
        ("task", context.task.as_deref()),
        (
            "instruction",
            context.instruction_sha256.as_deref().map(short),
        ),
        ("client session", context.client_session.as_deref()),
    ]
    .into_iter()
    .filter_map(|(name, value)| value.map(|value| format!("{name} {value}")))
    .collect();
    if parts.is_empty() {
        "none".to_owned()
    } else {
        parts.join(", ")
    }
}

/// `provenance show`'s record.
#[derive(Serialize)]
struct Shown {
    #[serde(flatten)]
    record: ProvenanceRecord,
    /// Set when the revision's commit had no record of its own: the change
    /// whose newest record this is.
    #[serde(skip_serializing_if = "Option::is_none")]
    latest_of: Option<String>,
}

impl TextRender for Shown {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let record = &self.record;
        if let Some(change) = &self.latest_of {
            writeln!(
                w,
                "(the commit has no record of its own; latest record of change {})",
                short(change)
            )?;
        }
        writeln!(
            w,
            "commit {}  change {}  {}  {}",
            short(&record.commit_id),
            short(&record.change_id),
            record.kind,
            record.recorded_at.as_deref().unwrap_or("-")
        )?;
        writeln!(
            w,
            "workspace {}  session {}  record {}",
            short(&record.workspace_id),
            record.session_id,
            record.sequence
        )?;
        writeln!(
            w,
            "signature: {}",
            record.signature.as_deref().unwrap_or("unknown")
        )?;
        writeln!(w, "identity: {}", identity_text(record))?;
        writeln!(w, "declared: {}", context_text(record))?;
        if let Some(envelope) = &record.envelope {
            writeln!(w, "envelope: {envelope}")?;
        }
        writeln!(
            w,
            "declared fields are claims; the signature says this daemon recorded the commit in this session; policy decides whom to trust"
        )
    }
}

impl TextRow for ProvenanceRecord {
    const HEADER: &'static [&'static str] = &[
        "COMMIT",
        "CHANGE",
        "KIND",
        "RECORDED",
        "IDENTITY",
        "MODEL",
        "TASK",
        "INSTRUCTION",
    ];

    fn row(&self) -> Vec<String> {
        let identity = self.identity.as_ref();
        let who = match identity.and_then(|identity| identity.key.as_ref()) {
            Some(key) => format!("{}:{}", key.identity, key.actor.as_deref().unwrap_or("-")),
            None => identity
                .and_then(|identity| identity.status.clone())
                .unwrap_or_else(|| "-".to_owned()),
        };
        let dash = |value: Option<&str>| value.unwrap_or("-").to_owned();
        vec![
            short(&self.commit_id).to_owned(),
            short(&self.change_id).to_owned(),
            self.kind.clone(),
            dash(self.recorded_at.as_deref()),
            who,
            dash(self.context.model.as_deref()),
            dash(self.context.task.as_deref()),
            dash(
                self.context
                    .instruction_sha256
                    .as_deref()
                    .map(|sha| sha.get(..8).unwrap_or(sha)),
            ),
        ]
    }
}

/// `provenance declare`'s result.
#[derive(Serialize)]
struct Declared {
    #[serde(flatten)]
    result: ProvenanceDeclared,
    #[serde(skip)]
    workspace: String,
    #[serde(skip)]
    fields: Vec<&'static str>,
}

impl TextRender for Declared {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let fields = if self.fields.is_empty() {
            "nothing changed".to_owned()
        } else {
            self.fields.join(", ")
        };
        let snapshot = match &self.result.snapshot_op {
            Some(op) => format!("snapshot {}", short(op)),
            None => "(nothing to snapshot)".to_owned(),
        };
        writeln!(
            w,
            "declared for workspace {}: {fields}; {snapshot}",
            short(&self.workspace)
        )
    }
}

/// The `ws inspect` line of a workspace's provenance session.
pub(crate) fn session_line(session: &ProvenanceSession) -> String {
    let identity = match (&session.identity, &session.actor) {
        (Some(identity), Some(actor)) => format!("{identity}:{actor}"),
        (None, _) => "unbound".to_owned(),
        (Some(identity), None) => identity.clone(),
    };
    let context = &session.context;
    let mut parts = vec![format!("session {} {identity}", session.session_id)];
    for (name, value) in [
        ("model", context.model.as_deref()),
        ("task", context.task.as_deref()),
        (
            "instruction",
            context
                .instruction_sha256
                .as_deref()
                .map(|sha| sha.get(..8).unwrap_or(sha)),
        ),
    ] {
        if let Some(value) = value {
            parts.push(format!("{name} {value}"));
        }
    }
    parts.push(format!("{} records", session.records));
    if session.missing_records > 0 {
        parts.push(format!("{} missing", session.missing_records));
    }
    if let Some(problem) = &session.problem {
        parts.push(format!("unbound: {problem}"));
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instruction_files_hash_their_exact_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("prompt.txt");
        std::fs::write(&path, "add a parser test\n").unwrap();
        assert_eq!(
            instruction_digest(&path).unwrap(),
            reef_client::evidence::sha256_hex(b"add a parser test\n"),
            "no newline trimmed"
        );
    }

    #[test]
    fn session_flags_declare_only_what_is_given() {
        let flags = SessionFlags::default();
        assert_eq!(flags.declaration().unwrap(), None);
        let flags = SessionFlags {
            model: Some(" m/1 ".into()),
            actor: Some("human".into()),
            ..SessionFlags::default()
        };
        let declaration = flags.declaration().unwrap().unwrap();
        assert_eq!(declaration.model.as_deref(), Some("m/1"));
        assert_eq!(declaration.actor(), v1::SessionActor::Human);
        assert_eq!(flags.env()[0], (MODEL_ENV, " m/1 ".to_owned()));
        assert!(!flags.only_from_env());
    }
}
