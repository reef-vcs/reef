//! `reef comment` (R3.12, ADR 0069): notes and instructions on changes.
//!
//! A note is data for whoever reads it. An instruction is work for the
//! session that made the change: it is written with an estimate of what
//! Reef recorded about its cost, confirmed in a second step (or in one,
//! within ceilings), and taken by that session with `reef comment next`,
//! which a harness hook or an agent runs, and which can wait for one. The
//! session finishes it by proposing the change again, or says it could not
//! with `reef comment fail`. Reef starts no agent.
//!
//! Text from a comment is data written by someone else: every text form
//! escapes control characters, and an instruction handed to a model is
//! framed with who wrote it.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, ClaimInstructionRequest, CreateCommentRequest, GetCommentRequest, ListCommentsRequest,
    OperationFilter, OperationKind, SubscribeOperationsRequest, UpdateInstructionRequest,
};
use reef_client::json::{self, RemoteCaller, reverse_hex};
use schemars::JsonSchema;
use serde::Serialize;

use super::advice::AdviceCtx;
use super::ambient::Ambient;
use super::changes::short;
use super::conflict::clean;
use super::input::{Fields, Input, Rules};
use super::out::{TextRender, TextRow, block};
use super::proposal::ActorArg;
use super::scope::{self, ScopeArgs};
use super::{CliError, Ctx, Global, Out, connect};

/// The variable a client session is read from when no harness names one.
pub(crate) const CLIENT_SESSION_ENV: &str = "REEF_CLIENT_SESSION";

/// The bytes of text the brief form keeps.
const BRIEF_TEXT: usize = 200;

/// The hook's whole deadline, connecting included.
const HOOK_DEADLINE: Duration = Duration::from_secs(2);

/// `reef comment`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: CommentCommand,
}

#[derive(Debug, clap::Subcommand)]
enum CommentCommand {
    /// Write a note, or an instruction for the session that made the change.
    #[command(
        after_help = "Example: reef comment add @- --instruction -m 'rename foo to bar'   # then reef comment confirm <id>"
    )]
    Add(AddArgs),
    /// List the comments of a change, oldest first (--all for the repository's).
    #[command(visible_alias = "ls")]
    List(ListArgs),
    /// Show a comment, its instruction's state and estimate, and its replies.
    Show(IdArgs),
    /// Confirm an instruction after reading its estimate: its session may take it.
    Confirm(IdArgs),
    /// Cancel an instruction before it runs.
    Cancel(MessageArgs),
    /// Take the next instruction for this session, and print it.
    #[command(
        after_help = "Example: reef comment next --wait 10m   # after reef propose, in the agent's shell"
    )]
    Next(NextArgs),
    /// Say an instruction you took has started.
    Start(IdArgs),
    /// Say an instruction is done (proposing the change again also does).
    Done(MessageArgs),
    /// Say an instruction could not be done, and why.
    Fail(FailArgs),
    /// Hand an instruction you took back, for its session to take again.
    Release(IdArgs),
}

/// `reef comment add`.
#[derive(Debug, clap::Args)]
#[expect(clippy::struct_excessive_bools, reason = "clap flags")]
pub(crate) struct AddArgs {
    /// The change.
    #[arg(value_name = "REV", default_value = "@")]
    revision: String,
    /// The text.
    #[arg(
        short = 'm',
        long = "message",
        value_name = "TEXT",
        conflicts_with = "file"
    )]
    message: Option<String>,
    /// Read the text from a file (`-` for stdin).
    #[arg(short = 'F', long, value_name = "FILE")]
    file: Option<PathBuf>,
    /// An instruction for the session that made the change, not a note.
    #[arg(long)]
    instruction: bool,
    /// The file it is about, repository-relative.
    #[arg(long, value_name = "PATH")]
    path: Option<String>,
    /// The lines it is about: N or N-M, 1-based.
    #[arg(long, value_name = "N[-M]", requires = "path")]
    lines: Option<String>,
    /// The lines are in the parent's version of the file (removed lines).
    #[arg(long, requires = "path")]
    old: bool,
    /// The comment it replies to.
    #[arg(long, value_name = "ID")]
    reply_to: Option<String>,
    /// Refuse unless the revision still names this commit (a full id).
    #[arg(long, value_name = "ID")]
    expected_commit: Option<String>,
    /// Who you are, as recorded [default: agent when an agent name is set, else human].
    #[arg(long, value_enum)]
    actor: Option<ActorArg>,
    /// With --instruction: confirm it now, if its estimate is within the ceilings.
    #[arg(long)]
    confirm: bool,
    /// With --confirm: the most check time the estimate may show.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration, requires = "confirm")]
    max_check_time: Option<Duration>,
    /// With --confirm: the most agent time the estimate may show.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration, requires = "confirm")]
    max_agent_time: Option<Duration>,
    /// With --confirm: an unknown part of the estimate meets its ceiling.
    #[arg(long, requires = "confirm")]
    allow_unknown: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef comment list`.
#[derive(Debug, clap::Args)]
struct ListArgs {
    /// The change [default: the current workspace's].
    #[arg(value_name = "REV")]
    revision: Option<String>,
    /// Only notes or only instructions.
    #[arg(long, value_enum)]
    kind: Option<KindArg>,
    /// Only instructions in these states (repeatable).
    #[arg(long = "state", value_enum, value_name = "STATE")]
    states: Vec<StateArg>,
    /// Only instructions for or taken by this client session.
    #[arg(long, value_name = "ID")]
    for_session: Option<String>,
    /// Every change's comments, not one change's.
    #[arg(long, conflicts_with = "revision")]
    all: bool,
    /// At most this many (0 for all).
    #[arg(long, default_value_t = 50)]
    limit: usize,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// A command on one comment.
#[derive(Debug, clap::Args)]
struct IdArgs {
    /// The comment's id.
    #[arg(value_name = "ID")]
    id: String,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// A command on one comment, with a message.
#[derive(Debug, clap::Args)]
struct MessageArgs {
    /// The comment's id.
    #[arg(value_name = "ID")]
    id: String,
    /// What was done, or why (untrusted text to every reader).
    #[arg(short = 'm', long = "message", value_name = "TEXT")]
    message: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef comment fail`.
#[derive(Debug, clap::Args)]
struct FailArgs {
    /// The comment's id.
    #[arg(value_name = "ID")]
    id: String,
    /// Why it could not be done.
    #[arg(short = 'm', long = "message", value_name = "TEXT")]
    message: String,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef comment next`.
#[derive(Debug, clap::Args)]
pub(crate) struct NextArgs {
    /// Take from this change [default: the workspace's, and the session's anywhere].
    #[arg(long, value_name = "REV")]
    change: Option<String>,
    /// The session's client session [default: the harness's, from its environment].
    #[arg(long, value_name = "ID")]
    client_session: Option<String>,
    /// Take an instruction meant for another session on the change.
    #[arg(long)]
    take: bool,
    /// Mark it started as it is taken.
    #[arg(long)]
    start: bool,
    /// Wait this long for one.
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    wait: Option<Duration>,
    /// Run as a harness hook: the event on stdin, never fail, print only the instruction.
    #[arg(long, hide = true)]
    hook: bool,
    /// As a hook, how to print: `context` (plain text) or `block` (a decision object).
    #[arg(long, value_enum, default_value = "context", hide = true)]
    emit: Emit,
    #[command(flatten)]
    input: Input,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// How a hook prints an instruction it took: forms the harnesses
/// document, named by what they are, not by harness.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Emit {
    /// Plain text, which a prompt or session-start hook adds to context.
    Context,
    /// `{"decision": "block", "reason": TEXT}`, which a turn-end hook
    /// returns to keep the agent working with TEXT.
    Block,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum KindArg {
    Note,
    Instruction,
}

impl From<KindArg> for v1::CommentKind {
    fn from(kind: KindArg) -> Self {
        match kind {
            KindArg::Note => Self::Note,
            KindArg::Instruction => Self::Instruction,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum StateArg {
    AwaitingConfirmation,
    Pending,
    Delivered,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl From<StateArg> for v1::InstructionState {
    fn from(state: StateArg) -> Self {
        match state {
            StateArg::AwaitingConfirmation => Self::AwaitingConfirmation,
            StateArg::Pending => Self::Pending,
            StateArg::Delivered => Self::Delivered,
            StateArg::Running => Self::Running,
            StateArg::Done => Self::Done,
            StateArg::Failed => Self::Failed,
            StateArg::Cancelled => Self::Cancelled,
        }
    }
}

fn channel_name(value: i32) -> Option<&'static str> {
    Some(match v1::DeliveryChannel::try_from(value) {
        Ok(v1::DeliveryChannel::Unspecified) => return None,
        Ok(v1::DeliveryChannel::Cli) => "cli",
        Ok(v1::DeliveryChannel::Hook) => "hook",
        Ok(v1::DeliveryChannel::Mcp) => "mcp",
        Ok(v1::DeliveryChannel::Sdk) => "sdk",
        Err(_) => json::UNKNOWN,
    })
}

fn source_name(value: i32) -> Option<&'static str> {
    Some(match v1::TargetSource::try_from(value) {
        Ok(v1::TargetSource::Unspecified) => return None,
        Ok(v1::TargetSource::Proposal) => "proposal",
        Ok(v1::TargetSource::Provenance) => "provenance",
        Ok(v1::TargetSource::None) => "none",
        Err(_) => json::UNKNOWN,
    })
}

#[expect(
    clippy::unnecessary_wraps,
    reason = "the shape `reef_client::schema::names` takes"
)]
fn side_name(value: i32) -> Option<&'static str> {
    Some(match v1::LineSide::try_from(value) {
        Ok(v1::LineSide::Old) => "old",
        Ok(v1::LineSide::New | v1::LineSide::Unspecified) => "new",
        Err(_) => json::UNKNOWN,
    })
}

macro_rules! names_schema {
    ($($schema:ident = $name:path;)*) => {$(
        fn $schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
            reef_client::schema::names($name, true)
        }
    )*};
}

names_schema! {
    kind_schema = json::comment_kind_name;
    state_schema = json::instruction_state_name;
    cause_schema = json::instruction_cause_name;
    actor_schema = json::transition_actor_name;
    channel_schema = channel_name;
    source_schema = source_name;
    side_schema = side_name;
}

fn text(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_owned())
}

fn time(value: Option<&prost_types::Timestamp>) -> Option<String> {
    value.map(json::rfc3339)
}

fn name(value: Option<&'static str>) -> Option<String> {
    value.map(str::to_owned)
}

/// Where in a change a comment points.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Anchor {
    /// Repository-relative.
    path: String,
    /// `new` (the commit's file) or `old` (its parent's).
    #[schemars(schema_with = "side_schema")]
    side: Option<String>,
    /// The first line, 1-based; absent for the whole file.
    start_line: Option<u32>,
    /// The last line.
    end_line: Option<u32>,
}

/// Who wrote a comment, as recorded.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Author {
    /// The call's agent.
    agent: Option<String>,
    /// `human` or `agent`, as declared.
    #[schemars(schema_with = "actor_schema")]
    actor: Option<String>,
    /// The operator identity recorded.
    identity: Option<String>,
    /// The provenance session of the workspace the call named.
    session_id: Option<String>,
    /// The remote token the call came through.
    #[serde(skip_serializing_if = "Option::is_none")]
    remote: Option<RemoteCaller>,
}

/// What Reef recorded that predicts an instruction's cost. Model tokens
/// and price are not visible to Reef.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Estimate {
    /// SHA-256 of the estimate.
    digest: String,
    /// When it was computed.
    computed_at: Option<String>,
    /// Whether a check of the change's tree finished.
    checks_known: bool,
    /// How many checks.
    checks: u32,
    /// Their wall time, in milliseconds.
    check_wall_ms: u64,
    /// Their CPU time, in milliseconds.
    check_cpu_ms: u64,
    /// Whether earlier instructions give an agent time.
    agent_known: bool,
    /// The median delivery-to-done time of earlier instructions, in
    /// milliseconds.
    agent_wall_ms: u64,
    /// Of how many.
    agent_samples: u32,
    /// `change`, `session` or `repository`.
    agent_basis: Option<String>,
    /// The target session's declared model.
    model: Option<String>,
    /// The target session's declared harness.
    harness: Option<String>,
    /// What is unknown, a sentence each.
    unknown: Vec<String>,
}

/// The session an instruction is for.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Target {
    /// `proposal`, `provenance` or `none`.
    #[schemars(schema_with = "source_schema")]
    source: Option<String>,
    /// The provenance session.
    session_id: Option<String>,
    /// The harness's own session id.
    client_session: Option<String>,
    /// The harness it declared.
    harness: Option<String>,
    /// Its agent.
    agent: Option<String>,
    /// Its workspace.
    workspace_id: Option<String>,
    /// When the client session was last seen; not whether it runs.
    last_seen_at: Option<String>,
}

/// An instruction's state, route and outcome.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Instruction {
    /// `awaiting_confirmation`, `pending`, `delivered`, `running`, `done`,
    /// `failed` or `cancelled`.
    #[schemars(schema_with = "state_schema")]
    state: Option<String>,
    /// `request`, `reproposed`, `change_final` or `expired`.
    #[schemars(schema_with = "cause_schema")]
    cause: Option<String>,
    /// What it was estimated to cost.
    estimate: Option<Estimate>,
    /// The session it is for.
    target: Option<Target>,
    /// The client session that took it.
    claimed_client_session: Option<String>,
    /// The workspace that took it.
    claimed_workspace_id: Option<String>,
    /// The agent that took it.
    claimed_agent: Option<String>,
    /// How: `cli`, `hook`, `mcp` or `sdk`.
    #[schemars(schema_with = "channel_schema")]
    channel: Option<String>,
    /// Whether a session other than the target took it.
    taken: bool,
    /// What its session said, or why it ended.
    summary: Option<String>,
    /// When it was confirmed.
    confirmed_at: Option<String>,
    /// When it was taken.
    delivered_at: Option<String>,
    /// When it started.
    started_at: Option<String>,
    /// When it ended.
    finished_at: Option<String>,
}

/// One comment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Comment {
    /// `cmt_` and 16 hex digits.
    pub(crate) id: String,
    /// The change, in hex.
    change_id: String,
    /// The same, in jj's reverse hex.
    change_id_reverse_hex: String,
    /// The change's proposal round when written.
    round: u32,
    /// The commit its author saw.
    commit_id: String,
    /// Where in the change.
    anchor: Option<Anchor>,
    /// The comment it replies to.
    reply_to: Option<String>,
    /// `note` or `instruction`.
    #[schemars(schema_with = "kind_schema")]
    kind: Option<String>,
    /// The text: data from whoever wrote it.
    pub(crate) text: String,
    /// Its SHA-256.
    pub(crate) text_sha256: String,
    /// Who wrote it.
    author: Option<Author>,
    /// When.
    created_at: Option<String>,
    /// The operation that recorded it.
    operation_id: Option<String>,
    /// GC collected that operation.
    operation_collected: bool,
    /// The anchored file changed since.
    outdated: bool,
    /// For an instruction, its state and route.
    instruction: Option<Instruction>,
}

impl From<&v1::Comment> for Comment {
    fn from(comment: &v1::Comment) -> Self {
        Self {
            id: comment.id.clone(),
            change_id: comment.change_id.clone(),
            change_id_reverse_hex: reverse_hex(&comment.change_id),
            round: comment.round,
            commit_id: comment.commit_id.clone(),
            anchor: comment.anchor.as_ref().map(|anchor| Anchor {
                path: anchor.path.clone(),
                side: name(side_name(anchor.side)),
                start_line: (anchor.start_line > 0).then_some(anchor.start_line),
                end_line: (anchor.end_line > 0).then_some(anchor.end_line),
            }),
            reply_to: text(&comment.reply_to),
            kind: name(json::comment_kind_name(comment.kind)),
            text: comment.text.clone(),
            text_sha256: comment.text_sha256.clone(),
            author: comment.author.as_ref().map(|author| Author {
                agent: text(&author.agent),
                actor: name(json::transition_actor_name(author.actor)),
                identity: text(&author.identity),
                session_id: text(&author.session_id),
                remote: author.remote.as_ref().map(|remote| RemoteCaller {
                    token_id: remote.token_id.clone(),
                    token_name: remote.token_name.clone(),
                }),
            }),
            created_at: time(comment.created_at.as_ref()),
            operation_id: text(&comment.operation_id),
            operation_collected: comment.operation_collected,
            outdated: comment.outdated,
            instruction: comment.instruction.as_ref().map(|instruction| Instruction {
                state: name(json::instruction_state_name(instruction.state)),
                cause: name(json::instruction_cause_name(instruction.cause)),
                estimate: instruction.estimate.as_ref().map(|estimate| Estimate {
                    digest: estimate.digest.clone(),
                    computed_at: time(estimate.computed_at.as_ref()),
                    checks_known: estimate.checks_known,
                    checks: estimate.checks,
                    check_wall_ms: estimate.check_wall_ms,
                    check_cpu_ms: estimate.check_cpu_ms,
                    agent_known: estimate.agent_known,
                    agent_wall_ms: estimate.agent_wall_ms,
                    agent_samples: estimate.agent_samples,
                    agent_basis: text(&estimate.agent_basis),
                    model: text(&estimate.model),
                    harness: text(&estimate.harness),
                    unknown: estimate.unknown.clone(),
                }),
                target: instruction.target.as_ref().map(|target| Target {
                    source: name(source_name(target.source)),
                    session_id: text(&target.session_id),
                    client_session: text(&target.client_session),
                    harness: text(&target.harness),
                    agent: text(&target.agent),
                    workspace_id: text(&target.workspace_id),
                    last_seen_at: time(target.last_seen_at.as_ref()),
                }),
                claimed_client_session: text(&instruction.claimed_client_session),
                claimed_workspace_id: text(&instruction.claimed_workspace_id),
                claimed_agent: text(&instruction.claimed_agent),
                channel: name(channel_name(instruction.channel)),
                taken: instruction.taken,
                summary: text(&instruction.summary),
                confirmed_at: time(instruction.confirmed_at.as_ref()),
                delivered_at: time(instruction.delivered_at.as_ref()),
                started_at: time(instruction.started_at.as_ref()),
                finished_at: time(instruction.finished_at.as_ref()),
            }),
        }
    }
}

/// `reef comment add`'s result: the comment, and why `--confirm` did not
/// confirm it, when it did not.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Written {
    /// The comment.
    comment: Comment,
    /// Why the instruction is still awaiting confirmation.
    confirm_refused: Option<String>,
}

/// `reef comment show`'s result: the comment and its replies.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Thread {
    /// The comment.
    comment: Comment,
    /// Its replies, oldest first.
    replies: Vec<Comment>,
}

/// `reef comment next`'s result: the instruction taken, or none.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct Next {
    /// The instruction; absent when there was none to take.
    comment: Option<Comment>,
    /// The instruction framed for a model: who wrote it, then its text,
    /// then what to do when done.
    frame: Option<String>,
}

/// A comment in brief: `id`, `kind`, `change_id`, the instruction's
/// `state`, the text's first 200 bytes and whether it was cut, and the
/// estimate's known totals. Dropped: everything else; `-o json` has it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct BriefComment<'a> {
    id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    kind: Option<&'a str>,
    change_id: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    text: &'a str,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    check_wall_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    agent_wall_ms: Option<u64>,
}

impl<'a> From<&'a Comment> for BriefComment<'a> {
    fn from(comment: &'a Comment) -> Self {
        let mut end = comment.text.len().min(BRIEF_TEXT);
        while !comment.text.is_char_boundary(end) {
            end -= 1;
        }
        let estimate = comment
            .instruction
            .as_ref()
            .and_then(|instruction| instruction.estimate.as_ref());
        Self {
            id: &comment.id,
            kind: comment.kind.as_deref(),
            change_id: &comment.change_id,
            state: comment
                .instruction
                .as_ref()
                .and_then(|instruction| instruction.state.as_deref()),
            text: &comment.text[..end],
            truncated: end < comment.text.len(),
            check_wall_ms: estimate
                .filter(|estimate| estimate.checks_known)
                .map(|estimate| estimate.check_wall_ms),
            agent_wall_ms: estimate
                .filter(|estimate| estimate.agent_known)
                .map(|estimate| estimate.agent_wall_ms),
        }
    }
}

impl<'a> From<&'a Written> for BriefComment<'a> {
    fn from(written: &'a Written) -> Self {
        Self::from(&written.comment)
    }
}

impl<'a> From<&'a Thread> for BriefComment<'a> {
    fn from(thread: &'a Thread) -> Self {
        Self::from(&thread.comment)
    }
}

/// `next` in brief: the instruction's brief, or `{}`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub(crate) struct BriefNext<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    comment: Option<BriefComment<'a>>,
}

impl<'a> From<&'a Next> for BriefNext<'a> {
    fn from(next: &'a Next) -> Self {
        Self {
            comment: next.comment.as_ref().map(BriefComment::from),
        }
    }
}

/// An instruction framed for a model: the first line says who wrote it
/// and where; then the text; then what to do when done.
pub(crate) fn frame(comment: &v1::Comment) -> String {
    let author = comment.author.as_ref();
    let who = author
        .map(|author| {
            if author.identity.is_empty() {
                author.agent.clone()
            } else {
                author.identity.clone()
            }
        })
        .filter(|who| !who.is_empty())
        .unwrap_or_else(|| "someone".to_owned());
    let actor = author
        .and_then(|author| json::transition_actor_name(author.actor))
        .unwrap_or("unknown");
    let mut header = format!(
        "Reef instruction {} from {} ({actor}) on change {}, round {}",
        comment.id,
        clean(&who),
        short(&reverse_hex(&comment.change_id)),
        comment.round
    );
    if let Some(anchor) = comment.anchor.as_ref().filter(|a| !a.path.is_empty()) {
        let _ = write!(header, ", at {}", clean(&anchor.path));
        if anchor.start_line > 0 {
            let _ = write!(header, ":{}", anchor.start_line);
            if anchor.end_line > anchor.start_line {
                let _ = write!(header, "-{}", anchor.end_line);
            }
        }
    }
    format!(
        "{header}:\n\n{}\n\nWhen done, propose the change again (reef propose), or run reef comment fail {} -m WHY.",
        comment.text, comment.id
    )
}

/// `reef comment`.
pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        CommentCommand::Add(args) => Box::pin(add(ctx, args)).await,
        CommentCommand::List(args) => Box::pin(list(ctx, args)).await,
        CommentCommand::Show(args) => Box::pin(show(ctx, args)).await,
        // Both move to PENDING, each from its own state, which it names so
        // neither can do the other's move.
        CommentCommand::Confirm(args) => {
            Box::pin(update(
                ctx,
                args.id,
                (
                    v1::InstructionState::Pending,
                    v1::InstructionState::AwaitingConfirmation,
                ),
                None,
                args.scope,
            ))
            .await
        }
        CommentCommand::Release(args) => {
            Box::pin(update(
                ctx,
                args.id,
                (
                    v1::InstructionState::Pending,
                    v1::InstructionState::Delivered,
                ),
                None,
                args.scope,
            ))
            .await
        }
        CommentCommand::Cancel(args) => {
            Box::pin(update(
                ctx,
                args.id,
                (
                    v1::InstructionState::Cancelled,
                    v1::InstructionState::Unspecified,
                ),
                args.message,
                args.scope,
            ))
            .await
        }
        CommentCommand::Start(args) => {
            Box::pin(update(
                ctx,
                args.id,
                (
                    v1::InstructionState::Running,
                    v1::InstructionState::Unspecified,
                ),
                None,
                args.scope,
            ))
            .await
        }
        CommentCommand::Done(args) => {
            Box::pin(update(
                ctx,
                args.id,
                (
                    v1::InstructionState::Done,
                    v1::InstructionState::Unspecified,
                ),
                args.message,
                args.scope,
            ))
            .await
        }
        CommentCommand::Fail(args) => {
            Box::pin(update(
                ctx,
                args.id,
                (
                    v1::InstructionState::Failed,
                    v1::InstructionState::Unspecified,
                ),
                Some(args.message),
                args.scope,
            ))
            .await
        }
        CommentCommand::Next(args) => Box::pin(next(ctx, args)).await,
    }
}

/// Whether `args` asks for the hook mode, which `reef` runs before it
/// connects (`run_hook`).
pub(crate) fn is_hook(args: &Args) -> Option<&NextArgs> {
    match &args.command {
        CommentCommand::Next(next) if next.hook => Some(next),
        _ => None,
    }
}

/// `N` or `N-M`.
fn parse_lines(text: &str) -> Result<(u32, u32), CliError> {
    let bad = || CliError::Usage(format!("--lines {text}: give N or N-M, 1-based"));
    let (start, end) = match text.split_once('-') {
        Some((start, end)) => (start, end),
        None => (text, text),
    };
    let start: u32 = start.trim().parse().map_err(|_| bad())?;
    let end: u32 = end.trim().parse().map_err(|_| bad())?;
    if start == 0 || end < start {
        return Err(bad());
    }
    Ok((start, end))
}

fn millis(duration: Option<Duration>) -> u64 {
    duration.map_or(0, |duration| {
        u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
    })
}

#[expect(clippy::too_many_lines, reason = "the flags' checks, then the request")]
async fn add(ctx: &Ctx, args: AddArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        revision: Some(args.revision.clone()),
        ..AdviceCtx::new(&scope)
    };
    let body = match (&args.message, &args.file) {
        (Some(message), _) => message.clone(),
        (None, Some(path)) if path.as_os_str() == "-" => {
            let mut text = String::new();
            io::Read::read_to_string(&mut io::stdin(), &mut text).map_err(|source| {
                CliError::Io {
                    op: "read",
                    path: "stdin".into(),
                    source,
                }
            })?;
            text
        }
        (None, Some(path)) => std::fs::read_to_string(path).map_err(|source| CliError::Io {
            op: "read",
            path: path.clone(),
            source,
        })?,
        (None, None) => {
            return Err(CliError::Usage(
                "give the text with -m TEXT or -F FILE".to_owned(),
            ));
        }
    };
    if args.confirm && !args.instruction {
        return Err(CliError::Usage(
            "--confirm confirms an instruction: add --instruction".to_owned(),
        ));
    }
    if args.confirm && args.max_check_time.is_none() && args.max_agent_time.is_none() {
        return Err(CliError::Usage(
            "--confirm needs --max-check-time or --max-agent-time: a one-call confirmation is bounded"
                .to_owned(),
        ));
    }
    let (start_line, end_line) = match &args.lines {
        Some(lines) => parse_lines(lines)?,
        None => (0, 0),
    };
    let anchor = args.path.as_ref().map(|path| v1::CommentAnchor {
        path: path.clone(),
        side: if args.old {
            v1::LineSide::Old
        } else {
            v1::LineSide::New
        }
        .into(),
        start_line,
        end_line,
    });
    let kind = if args.instruction {
        v1::CommentKind::Instruction
    } else {
        v1::CommentKind::Note
    };
    let request = CreateCommentRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        revision: args.revision.clone(),
        expected_commit_id: args.expected_commit.clone().unwrap_or_default(),
        anchor,
        reply_to: args.reply_to.clone().unwrap_or_default(),
        kind: kind.into(),
        text: body,
        actor: args
            .actor
            .map_or(v1::TransitionActor::Unspecified, v1::TransitionActor::from)
            .into(),
        confirm: args.confirm,
        ceilings: args.confirm.then(|| v1::InstructionCeilings {
            max_check_wall_ms: millis(args.max_check_time),
            max_agent_wall_ms: millis(args.max_agent_time),
            allow_unknown: args.allow_unknown,
        }),
        skip_snapshot: false,
    };
    let response = Box::pin(ctx.call(ctx.client.create_comment(request)))
        .await
        .map_err(|error| advice.refine(error))?;
    let comment = response.comment.unwrap_or_default();
    let written = Written {
        comment: Comment::from(&comment),
        confirm_refused: text(&response.confirm_refused),
    };
    ctx.out.object_with(&written, BriefComment::from)?;
    if kind == v1::CommentKind::Instruction {
        let state = comment
            .instruction
            .as_ref()
            .map_or(v1::InstructionState::Unspecified, v1::Instruction::state);
        if state == v1::InstructionState::AwaitingConfirmation {
            ctx.out.note(&format!(
                "reef: read the estimate, then `reef comment confirm {}` lets its session take it",
                comment.id
            ));
        }
    }
    Ok(())
}

async fn list(ctx: &Ctx, args: ListArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let changes = if args.all {
        Vec::new()
    } else {
        let revision = args.revision.clone().unwrap_or_else(|| "@".to_owned());
        let change = ctx
            .call(ctx.client.get_change(v1::GetChangeRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                revision,
                skip_snapshot: true,
                ..v1::GetChangeRequest::default()
            }))
            .await
            .map_err(|error| advice.refine(error))?
            .change
            .unwrap_or_default();
        vec![change.change_id]
    };
    let listed = ctx
        .call(
            ctx.client.list_comments(
                ListCommentsRequest {
                    repo: scope.repo_id.clone(),
                    changes,
                    kinds: args
                        .kind
                        .map(|kind| vec![v1::CommentKind::from(kind).into()])
                        .unwrap_or_default(),
                    states: args
                        .states
                        .iter()
                        .map(|state| v1::InstructionState::from(*state).into())
                        .collect(),
                    client_session: args.for_session.clone().unwrap_or_default(),
                    ..ListCommentsRequest::default()
                },
                args.limit,
            ),
        )
        .await
        .map_err(|error| advice.refine(error))?;
    if listed.more {
        ctx.out.note(&format!(
            "reef: more comments than --limit {}; raise it, or 0 for all",
            args.limit
        ));
    }
    let comments: Vec<Comment> = listed.items.iter().map(Comment::from).collect();
    ctx.out.list_with(&comments, |comments| {
        comments.iter().map(BriefComment::from).collect::<Vec<_>>()
    })
}

async fn show(ctx: &Ctx, args: IdArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let response = ctx
        .call(ctx.client.get_comment(GetCommentRequest {
            repo: scope.repo_id.clone(),
            id: args.id.clone(),
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let thread = Thread {
        comment: Comment::from(&response.comment.unwrap_or_default()),
        replies: response.replies.iter().map(Comment::from).collect(),
    };
    ctx.out.object_with(&thread, BriefComment::from)
}

async fn update(
    ctx: &Ctx,
    id: String,
    (to, expected): (v1::InstructionState, v1::InstructionState),
    message: Option<String>,
    scope_args: ScopeArgs,
) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &scope_args).await?;
    let advice = AdviceCtx::new(&scope);
    let response = ctx
        .call(ctx.client.update_instruction(UpdateInstructionRequest {
            repo: scope.repo_id.clone(),
            id,
            to: to.into(),
            expected_state: expected.into(),
            summary: message.unwrap_or_default(),
            workspace: scope.workspace_id(),
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let comment = Comment::from(&response.comment.unwrap_or_default());
    if response.operation.is_none() {
        ctx.out.note("reef: already so; nothing recorded");
    }
    ctx.out.object_with(&comment, BriefComment::from)
}

/// The client session this process speaks for: the flag, else the
/// harness's environment, else [`CLIENT_SESSION_ENV`].
fn client_session(flag: Option<String>) -> String {
    flag.or_else(|| Ambient::current().client_session)
        .or_else(|| std::env::var(CLIENT_SESSION_ENV).ok())
        .map(|session| session.trim().to_owned())
        .unwrap_or_default()
}

/// Claims one instruction, or waits up to `wait` for one, following the
/// repository's comment operations.
async fn claim_or_wait(
    ctx: &Ctx,
    request: ClaimInstructionRequest,
    wait: Option<Duration>,
) -> Result<Option<v1::Comment>, CliError> {
    let first = ctx
        .call(ctx.client.claim_instruction(request.clone()))
        .await?;
    if first.comment.is_some() {
        return Ok(first.comment);
    }
    let Some(wait) = wait.filter(|wait| !wait.is_zero()) else {
        return Ok(None);
    };
    let deadline = Instant::now() + wait;
    let mut stream = ctx
        .call(ctx.client.subscribe_operations(SubscribeOperationsRequest {
            repos: vec![request.repo.clone()],
            filter: Some(OperationFilter {
                kinds: vec![OperationKind::Comment.into()],
                ..OperationFilter::default()
            }),
            start: None,
        }))
        .await?;
    // A confirm between the first claim and the subscription is not on
    // the stream: claim once more now that it is.
    let again = ctx
        .call(ctx.client.claim_instruction(request.clone()))
        .await?;
    if again.comment.is_some() {
        return Ok(again.comment);
    }
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(None);
        }
        let frame = match tokio::time::timeout(left, stream.message()).await {
            Err(_) | Ok(Ok(None)) => return Ok(None),
            Ok(Ok(Some(frame))) => frame,
            Ok(Err(status)) => {
                return Err(CliError::Daemon(reef_client::ClientError::from(status)));
            }
        };
        let confirmed = frame
            .operations
            .iter()
            .any(|operation| operation.comments.iter().any(confirmed_event));
        if confirmed {
            let claimed = ctx
                .call(ctx.client.claim_instruction(request.clone()))
                .await?;
            if claimed.comment.is_some() {
                return Ok(claimed.comment);
            }
        }
    }
}

/// Whether `event` confirms an instruction (a `confirm`, or an instruction
/// written confirmed): what a waiting session takes. A `release` is not:
/// the session that released one does not get it straight back.
pub(crate) fn confirmed_event(event: &v1::CommentEvent) -> bool {
    event.kind() == v1::CommentKind::Instruction
        && event.to() == v1::InstructionState::Pending
        && (event.event == "confirm" || event.event == "instruct")
}

/// Declares an instruction's hash as the instruction of every active
/// workspace of `session` (R3.44's fan-out), so the records its work
/// produces name it. Best effort.
pub(crate) async fn declare_instruction(ctx: &Ctx, session: &str, sha256: &str) {
    if session.is_empty() {
        return;
    }
    let workspaces = match super::declare_hook::session_workspaces(ctx, session).await {
        Ok(workspaces) => workspaces,
        Err(error) => {
            tracing::debug!(target: "reef", reason = error.reason(), "comment: the session's workspaces");
            return;
        }
    };
    for ws in workspaces {
        let request = v1::DeclareProvenanceRequest {
            repo: ws.repo_id.clone(),
            workspace: ws.id.clone(),
            declaration: Some(v1::ProvenanceDeclaration {
                instruction_sha256: Some(sha256.to_owned()),
                ..v1::ProvenanceDeclaration::default()
            }),
            // Declared for what comes next; a snapshot first would only
            // delay the hand-over.
            no_snapshot: true,
        };
        if let Err(error) = ctx.call(ctx.client.declare_provenance(request)).await {
            tracing::debug!(target: "reef", reason = error.reason(), "comment: declaring the instruction");
        }
    }
}

async fn next(ctx: &Ctx, args: NextArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let session = client_session(args.client_session.clone());
    let request = ClaimInstructionRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        client_session: session.clone(),
        change: args.change.clone().unwrap_or_default(),
        comment_id: String::new(),
        take: args.take,
        start: args.start,
        channel: v1::DeliveryChannel::Cli.into(),
    };
    let claimed = Box::pin(claim_or_wait(ctx, request, args.wait))
        .await
        .map_err(|error| advice.refine(error))?;
    if let Some(comment) = &claimed {
        let declared = comment
            .instruction
            .as_ref()
            .map(|instruction| instruction.claimed_client_session.clone())
            .filter(|session| !session.is_empty())
            .unwrap_or(session);
        declare_instruction(ctx, &declared, &comment.text_sha256).await;
    } else {
        ctx.out
            .note("reef: no instruction to take; --wait DURATION waits for one");
    }
    let next = Next {
        comment: claimed.as_ref().map(Comment::from),
        frame: claimed.as_ref().map(frame),
    };
    ctx.out.object_with(&next, BriefNext::from)
}

/// The fields a hook's event gives `next`.
const HOOK_FIELDS: &[&str] = &["client_session", "workspace"];

/// `reef comment next --hook`: what a harness's hook runs. It never fails
/// the event it runs on: it exits 0, prints nothing unless it took an
/// instruction, never starts a daemon, and gives up after two seconds.
pub(crate) async fn run_hook(global: &Global, args: &NextArgs) -> Result<(), CliError> {
    let global = Global {
        no_autostart: true,
        ..global.clone()
    };
    let deadline = tokio::time::Instant::now() + HOOK_DEADLINE;
    match Box::pin(tokio::time::timeout_at(deadline, hook(&global, args))).await {
        Ok(Ok(Some(taken))) => {
            // Printed as soon as it is taken; the declaration, which only
            // labels what comes next, gets what time is left.
            #[expect(clippy::print_stdout, reason = "a hook's stdout is its output")]
            {
                print!("{}", taken.printed);
            }
            let _ = io::stdout().flush();
            let declared = declare_instruction(&taken.ctx, &taken.session, &taken.sha256);
            if tokio::time::timeout_at(deadline, declared).await.is_err() {
                tracing::debug!(target: "reef", "comment hook: no time to declare the instruction");
            }
        }
        Ok(Ok(None)) => {}
        Ok(Err(error)) => {
            tracing::debug!(target: "reef", reason = error.reason(), "comment hook");
        }
        Err(_) => tracing::debug!(target: "reef", "comment hook: out of time"),
    }
    Ok(())
}

/// What the hook took: its output, and what declaring it needs.
struct Taken {
    printed: String,
    ctx: Ctx,
    session: String,
    sha256: String,
}

async fn hook(global: &Global, args: &NextArgs) -> Result<Option<Taken>, CliError> {
    let event = if args.input.reads_stdin() {
        Some(super::hook::read_event().await?)
    } else {
        None
    };
    let rules = Rules {
        known: HOOK_FIELDS,
        numbers: &[],
        invalid: |_, message| CliError::Usage(message),
    };
    let fields = Fields::with(event, &args.input.at, &rules)?;
    let session = client_session(fields.get("client_session", args.client_session.clone())?);
    let workspace = fields.get("workspace", args.scope.workspace.clone())?;
    let out = Out::detect(Some(super::Output::Json), true);
    let ctx = connect(global, &out).await?;
    let found = match workspace.as_deref() {
        Some(text) if !(super::remote_mode() && text.starts_with('/')) => {
            scope::find_workspace(&ctx, text).await.ok().flatten()
        }
        _ => None,
    };
    if session.is_empty() && found.is_none() {
        return Ok(None);
    }
    let repos: Vec<(String, String)> = match &found {
        Some(ws) => vec![(ws.repo_id.clone(), ws.id.clone())],
        None => ctx
            .call(ctx.client.list_repositories())
            .await?
            .into_iter()
            .map(|repo| (repo.id, String::new()))
            .collect(),
    };
    for (repo, workspace) in repos {
        // Taken as DELIVERED, not started: one the deadline cuts off before
        // it is printed can still be released and taken again.
        let claimed = ctx
            .call(ctx.client.claim_instruction(ClaimInstructionRequest {
                repo,
                workspace,
                client_session: session.clone(),
                channel: v1::DeliveryChannel::Hook.into(),
                ..ClaimInstructionRequest::default()
            }))
            .await;
        let claimed = match claimed {
            Ok(claimed) => claimed,
            Err(error) => {
                // Another repository may still have one.
                tracing::debug!(target: "reef", reason = error.reason(), "comment hook: a repository");
                continue;
            }
        };
        if let Some(comment) = claimed.comment {
            let text = frame(&comment);
            let printed = match args.emit {
                Emit::Context => format!("{text}\n"),
                Emit::Block => format!(
                    "{}\n",
                    serde_json::json!({"decision": "block", "reason": text})
                ),
            };
            return Ok(Some(Taken {
                printed,
                ctx,
                session,
                sha256: comment.text_sha256,
            }));
        }
    }
    Ok(None)
}

fn duration_words(millis: u64) -> String {
    humantime::format_duration(Duration::from_secs(millis / 1000)).to_string()
}

/// An estimate in lines: each known part, then each unknown one.
fn estimate_lines(estimate: &Estimate) -> Vec<String> {
    let mut lines = Vec::new();
    if estimate.checks_known {
        lines.push(format!(
            "checks: {} last run, {} wall, {} CPU",
            estimate.checks,
            duration_words(estimate.check_wall_ms),
            duration_words(estimate.check_cpu_ms)
        ));
    }
    if estimate.agent_known {
        lines.push(format!(
            "agent: {} (median of {} earlier instruction(s), by {})",
            duration_words(estimate.agent_wall_ms),
            estimate.agent_samples,
            estimate.agent_basis.as_deref().unwrap_or("-")
        ));
    }
    if let Some(model) = &estimate.model {
        lines.push(format!("model: {} (declared)", clean(model)));
    }
    for unknown in &estimate.unknown {
        lines.push(format!("unknown: {unknown}"));
    }
    lines
}

fn comment_pairs(comment: &Comment) -> Vec<(&'static str, String)> {
    let mut pairs = vec![
        ("comment", comment.id.clone()),
        ("kind", comment.kind.clone().unwrap_or_default()),
        (
            "change",
            format!(
                "{} (round {})",
                short(&comment.change_id_reverse_hex),
                comment.round
            ),
        ),
    ];
    if let Some(author) = &comment.author {
        let who = author
            .identity
            .clone()
            .or_else(|| author.agent.clone())
            .unwrap_or_else(|| "-".to_owned());
        pairs.push((
            "by",
            format!(
                "{} ({})",
                clean(&who),
                author.actor.as_deref().unwrap_or("-")
            ),
        ));
    }
    if let Some(anchor) = &comment.anchor {
        let mut at = clean(&anchor.path);
        if let Some(start) = anchor.start_line {
            let _ = write!(at, ":{start}");
            if let Some(end) = anchor.end_line.filter(|end| *end > start) {
                let _ = write!(at, "-{end}");
            }
        }
        if anchor.side.as_deref() == Some("old") {
            at.push_str(" (old)");
        }
        if comment.outdated {
            at.push_str(" (outdated)");
        }
        pairs.push(("at", at));
    }
    if let Some(reply_to) = &comment.reply_to {
        pairs.push(("reply to", reply_to.clone()));
    }
    if let Some(instruction) = &comment.instruction {
        let mut state = instruction.state.clone().unwrap_or_default();
        if let Some(cause) = instruction.cause.as_deref().filter(|c| *c != "request") {
            let _ = write!(state, " ({cause})");
        }
        pairs.push(("state", state));
        if let Some(target) = &instruction.target {
            let session = target
                .client_session
                .clone()
                .or_else(|| target.session_id.clone())
                .unwrap_or_else(|| "any session on the change".to_owned());
            pairs.push(("for", clean(&session)));
            if let Some(seen) = &target.last_seen_at {
                pairs.push(("last seen", seen.clone()));
            }
        }
        if let Some(session) = &instruction.claimed_client_session {
            let taken = if instruction.taken { " (taken)" } else { "" };
            pairs.push(("taken by", format!("{}{taken}", clean(session))));
        }
        if let Some(summary) = &instruction.summary {
            pairs.push(("summary", clean(summary.lines().next().unwrap_or_default())));
        }
        if let Some(estimate) = &instruction.estimate {
            for line in estimate_lines(estimate) {
                pairs.push(("estimate", line));
            }
        }
    }
    pairs
}

impl TextRender for Comment {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        block(w, &comment_pairs(self))?;
        writeln!(w)?;
        for line in self.text.lines() {
            writeln!(w, "  {}", clean(line))?;
        }
        Ok(())
    }
}

impl TextRender for Written {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        self.comment.render(w)?;
        if let Some(why) = &self.confirm_refused {
            writeln!(w, "\nnot confirmed: {why}")?;
        }
        Ok(())
    }
}

impl TextRender for Thread {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        self.comment.render(w)?;
        for reply in &self.replies {
            writeln!(w)?;
            reply.render(w)?;
        }
        Ok(())
    }
}

impl TextRender for Next {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        if let Some(frame) = &self.frame {
            for line in frame.lines() {
                writeln!(w, "{}", clean(line))?;
            }
        }
        Ok(())
    }
}

impl TextRow for Comment {
    const HEADER: &'static [&'static str] = &["COMMENT", "KIND", "STATE", "BY", "AT", "TEXT"];

    fn row(&self) -> Vec<String> {
        let who = self
            .author
            .as_ref()
            .and_then(|author| author.identity.clone().or_else(|| author.agent.clone()))
            .unwrap_or_else(|| "-".to_owned());
        vec![
            self.id.clone(),
            self.kind.clone().unwrap_or_default(),
            self.instruction
                .as_ref()
                .and_then(|instruction| instruction.state.clone())
                .unwrap_or_else(|| "-".to_owned()),
            clean(&who),
            self.anchor
                .as_ref()
                .map_or_else(|| "-".to_owned(), |anchor| clean(&anchor.path)),
            clean(self.text.lines().next().unwrap_or_default()),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_parse() {
        assert_eq!(parse_lines("3").unwrap(), (3, 3));
        assert_eq!(parse_lines("3-5").unwrap(), (3, 5));
        for bad in ["0", "5-3", "x", "1-"] {
            assert!(parse_lines(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn frames_say_who_and_what_next() {
        let comment = v1::Comment {
            id: "cmt_0123456789abcdef".into(),
            change_id: "0123456789abcdef0123456789abcdef".into(),
            round: 2,
            text: "rename foo".into(),
            author: Some(v1::CommentAuthor {
                identity: "alice".into(),
                actor: v1::TransitionActor::Human.into(),
                ..v1::CommentAuthor::default()
            }),
            anchor: Some(v1::CommentAnchor {
                path: "src/a.rs".into(),
                start_line: 3,
                end_line: 4,
                ..v1::CommentAnchor::default()
            }),
            ..v1::Comment::default()
        };
        let framed = frame(&comment);
        assert!(framed.starts_with(
            "Reef instruction cmt_0123456789abcdef from alice (human) on change zyxwvutsrqpo, round 2, at src/a.rs:3-4:\n\nrename foo\n"
        ), "{framed}");
        assert!(framed.ends_with("reef comment fail cmt_0123456789abcdef -m WHY."));
    }

    #[test]
    fn brief_cuts_at_a_character() {
        let comment = Comment::from(&v1::Comment {
            id: "cmt_1".into(),
            text: "é".repeat(150),
            ..v1::Comment::default()
        });
        let brief = BriefComment::from(&comment);
        assert!(brief.truncated);
        assert!(brief.text.len() <= BRIEF_TEXT);
    }
}
