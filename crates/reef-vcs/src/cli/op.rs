//! `reef op log`, `op show` and `undo` (R2.27): the repository's operation
//! log, one operation with its effects, and the revert of one operation.
//! `undo` always names its operation: watchers and other agents record
//! operations every second, so "the last one" is rarely the caller's.
//!
//! `reef op watch` (R2.09) prints operations as they are recorded, from
//! `SubscribeOperations`, and always ends by itself unless `--follow`: an
//! agent's command must return (ADR 0025).

use std::time::Duration;

use reef_api::v1::subscribe_operations_request::Start;
use reef_api::v1::{
    self, ListOperationsRequest, OperationFilter, SubscribeOperationsRequest,
    SubscribeOperationsResponse, UndoOperationRequest,
};
use reef_client::ClientError;
use reef_client::json::{Operation, Undone};
use reef_error::{Advice, ErrorKind};
use tokio::time::Instant;

use super::advice::AdviceCtx;
use super::changes::{note_divergence, short, workspace_after};
use super::log::LOG_LIMIT;
use super::render_vcs::{OpLine, OpLines, OpShow};
use super::repo::missing;
use super::scope::{self, ScopeArgs, workspace_arg};
use super::stream::{Backoff, Stopped, run_until_stopped, until};
use super::{CliError, Ctx};

/// `reef op`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: OpCommand,
}

#[derive(Debug, clap::Subcommand)]
enum OpCommand {
    /// List the repository's operations, newest first.
    #[command(after_help = "Example: reef op log --kind describe --by bot-1 --limit 5")]
    Log(LogArgs),
    /// Show one operation and what it changed.
    Show {
        /// An operation id or unique prefix.
        #[arg(value_name = "OP")]
        operation: String,
        /// The repository (id, id prefix, or path) [default: the one you are in].
        #[arg(long, value_name = "REPO")]
        repo: Option<String>,
    },
    /// Print operations as they are recorded (JSON lines on a pipe).
    #[command(after_help = "Example: reef op watch --kind describe --count 1")]
    Watch(WatchArgs),
}

/// `reef op log`.
#[derive(Debug, clap::Args)]
struct LogArgs {
    #[command(flatten)]
    filter: FilterArgs,
    /// Include what each operation changed.
    #[arg(long)]
    effects: bool,
    /// Show at most this many; 0 for all. A cut is noted on stderr.
    #[arg(long, value_name = "N", default_value_t = LOG_LIMIT)]
    limit: usize,
    /// The repository (id, id prefix, or path) [default: the one you are in].
    #[arg(long, value_name = "REPO")]
    repo: Option<String>,
}

/// `reef op watch`.
#[derive(Debug, clap::Args)]
struct WatchArgs {
    #[command(flatten)]
    filter: FilterArgs,
    /// Include what each operation changed.
    #[arg(long)]
    effects: bool,
    /// Print the operations after this one first (id or unique prefix).
    #[arg(long, value_name = "OP")]
    after: Option<String>,
    /// Stop after printing this many operations.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u64).range(1..))]
    count: Option<u64>,
    /// Stop after this many seconds [default: 30]; 0 prints what --after replays.
    #[arg(long = "for", value_name = "SECONDS", conflicts_with = "follow")]
    seconds: Option<u64>,
    /// Keep watching until interrupted or --count.
    #[arg(long)]
    follow: bool,
    /// The repository (id, id prefix, or path) [default: the one you are in].
    #[arg(long, value_name = "REPO")]
    repo: Option<String>,
}

/// `op log`'s and `op watch`'s filter.
#[derive(Debug, clap::Args)]
struct FilterArgs {
    /// Only operations for this workspace (id, prefix, or path); repeatable.
    #[arg(long, value_name = "WS")]
    workspace: Vec<String>,
    /// Only operations that touched this change (hex or reverse-hex prefix); repeatable.
    #[arg(long, value_name = "CHANGE")]
    change: Vec<String>,
    /// Only operations by this agent; repeatable.
    #[arg(long, value_name = "AGENT")]
    by: Vec<String>,
    /// Only operations of this kind; repeatable.
    #[arg(long, value_enum, value_name = "KIND")]
    kind: Vec<KindArg>,
}

impl FilterArgs {
    fn filter(self) -> Result<OperationFilter, CliError> {
        Ok(OperationFilter {
            workspaces: self
                .workspace
                .iter()
                .map(|ws| workspace_arg(ws))
                .collect::<Result<_, _>>()?,
            changes: self.change,
            agents: self.by,
            kinds: self
                .kind
                .into_iter()
                .map(|kind| v1::OperationKind::from(kind).into())
                .collect(),
        })
    }
}

/// An operation kind, by its `reef.kind` name.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum KindArg {
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

impl From<KindArg> for v1::OperationKind {
    fn from(kind: KindArg) -> Self {
        match kind {
            KindArg::Snapshot => Self::Snapshot,
            KindArg::CreateWorkspace => Self::CreateWorkspace,
            KindArg::ReleaseWorkspace => Self::ReleaseWorkspace,
            KindArg::MoveWorkspace => Self::MoveWorkspace,
            KindArg::Describe => Self::Describe,
            KindArg::Abandon => Self::Abandon,
            KindArg::Undo => Self::Undo,
            KindArg::Edit => Self::Edit,
            KindArg::ImportRefs => Self::ImportRefs,
            KindArg::External => Self::External,
            KindArg::Other => Self::Other,
        }
    }
}

/// `reef undo`.
#[derive(Debug, clap::Args)]
pub(crate) struct UndoArgs {
    /// The operation to undo (id or unique prefix; `reef op log` lists them).
    #[arg(value_name = "OP")]
    operation: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        OpCommand::Log(args) => log(ctx, args).await,
        OpCommand::Show { operation, repo } => {
            let scope = scope::resolve_repo_only(ctx, repo.as_deref()).await?;
            let advice = AdviceCtx {
                operation_given: true,
                ..AdviceCtx::new(&scope)
            };
            let response = ctx
                .call(ctx.client.get_operation(&scope.repo_id, &operation))
                .await
                .map_err(|error| advice.refine(error))?;
            let operation = response.operation.ok_or_else(|| missing("operation"))?;
            ctx.out.object(&OpShow(Operation::from(&operation)))
        }
        OpCommand::Watch(args) => watch(ctx, args).await,
    }
}

async fn log(ctx: &Ctx, args: LogArgs) -> Result<(), CliError> {
    let scope = scope::resolve_repo_only(ctx, args.repo.as_deref()).await?;
    let advice = AdviceCtx::new(&scope);
    let filter = args.filter.filter()?;
    let request = ListOperationsRequest {
        repo: scope.repo_id.clone(),
        filter: Some(filter),
        include_effects: args.effects,
        ..ListOperationsRequest::default()
    };
    let limited = ctx
        .call(ctx.client.list_operations_limited(request, args.limit))
        .await
        .map_err(|error| advice.refine(error))?;
    if limited.more {
        ctx.out.note(&format!(
            "reef: showing the first {} operations; --limit 0 shows all",
            limited.items.len()
        ));
    }
    ctx.out.object(&OpLines(
        limited.items.iter().map(Operation::from).collect(),
    ))
}

pub(crate) async fn undo(ctx: &Ctx, args: UndoArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let Some(operation) = args.operation else {
        return Err(CliError::Refused {
            kind: ErrorKind::InvalidArgument,
            message: "name the operation to undo".to_owned(),
            advice: Some(Advice::run(
                "Name the operation to undo; `reef op log` lists them, newest first.",
                ["reef", "op", "log", "--repo", &scope.repo_id],
            )),
        });
    };
    let advice = AdviceCtx {
        operation_given: true,
        ..AdviceCtx::new(&scope)
    };
    let response = ctx
        .call(ctx.client.undo_operation(UndoOperationRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            operation_id: operation,
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let workspace = workspace_after(ctx, scope.workspace_ref()).await;
    let undone = Undone::from_response(&response, workspace);
    let note = match &undone.operation {
        Some(op) => format!(
            "undid {} as {}",
            short(&undone.undone_operation_id),
            short(&op.id)
        ),
        None => format!("nothing to undo in {}", short(&undone.undone_operation_id)),
    };
    ctx.out.done(&undone, &note)?;
    note_divergence(ctx, undone.workspace.as_ref());
    Ok(())
}

/// How long `op watch` runs without `--for` or `--follow`: the MCP wait
/// tool's default.
const WATCH_FOR: Duration = Duration::from_secs(30);

async fn watch(ctx: &Ctx, args: WatchArgs) -> Result<(), CliError> {
    let scope = scope::resolve_repo_only(ctx, args.repo.as_deref()).await?;
    let advice = AdviceCtx {
        operation_given: args.after.is_some(),
        ..AdviceCtx::new(&scope)
    };
    let request = SubscribeOperationsRequest {
        repos: vec![scope.repo_id.clone()],
        filter: Some(args.filter.filter()?),
        start: args.after.clone().map(Start::AfterOperation),
    };
    let seconds = args.seconds.map_or(WATCH_FOR, Duration::from_secs);
    // `--for 0` ends at the first live frame instead.
    let replay_only = args.seconds == Some(0);
    let deadline = (!args.follow && !replay_only).then(|| Instant::now() + seconds);
    let mut watcher = Watcher {
        ctx,
        effects: args.effects,
        count: args
            .count
            .map(|count| usize::try_from(count).unwrap_or(usize::MAX)),
        replay_only,
        printed: 0,
        heads: Vec::new(),
    };
    let stopped = run_until_stopped(watcher.run(request, deadline))
        .await
        .map_err(|error| advice.refine(error))?;
    if let Stopped::Time = stopped
        && let Some(at) = watcher.heads.last()
    {
        let at = short(at);
        ctx.out.note(&format!(
            "reef: stopped after {} s at operation {at}; reef op watch --after {at} continues",
            seconds.as_secs()
        ));
    }
    Ok(())
}

/// One `op watch`: what it printed and where the stream stands.
struct Watcher<'a> {
    ctx: &'a Ctx,
    effects: bool,
    count: Option<usize>,
    /// `--for 0`: stop at the first live frame.
    replay_only: bool,
    printed: usize,
    /// The last frame's `head_operation_ids`.
    heads: Vec<String>,
}

impl Watcher<'_> {
    /// Subscribes and prints until it stops, resubscribing from the last
    /// frame's cursor after the daemon goes away.
    async fn run(
        &mut self,
        mut request: SubscribeOperationsRequest,
        deadline: Option<Instant>,
    ) -> Result<Stopped, CliError> {
        let mut backoff = Backoff::new();
        loop {
            let subscribed = tokio::select! {
                () = until(deadline) => return Ok(Stopped::Time),
                subscribed = self.ctx.call(self.ctx.client.subscribe_operations(request.clone())) => subscribed,
            };
            let error = match subscribed {
                Ok(mut stream) => {
                    loop {
                        let next = tokio::select! {
                            () = until(deadline) => return Ok(Stopped::Time),
                            next = stream.message() => next,
                        };
                        match next {
                            Ok(Some(frame)) => {
                                backoff.reset();
                                if let Some(stopped) = self.print(&frame)? {
                                    return Ok(stopped);
                                }
                                request.start = Some(Start::Cursor(frame.cursor));
                            }
                            // Ended without a status: the connection went.
                            Ok(None) => break None,
                            Err(status) => break Some(CliError::Daemon(ClientError::from(status))),
                        }
                    }
                }
                Err(error) => Some(error),
            };
            if let Some(error) = error
                && !backoff.retry(&error, deadline)
            {
                return Err(error);
            }
            if !backoff.wait(deadline).await {
                return Ok(Stopped::Time);
            }
        }
    }

    /// Prints a frame's operations; `Some` when the watch is done.
    fn print(&mut self, frame: &SubscribeOperationsResponse) -> Result<Option<Stopped>, CliError> {
        for operation in &frame.operations {
            let mut operation = Operation::from(operation);
            if !self.effects {
                operation.effects = None;
            }
            if !self.ctx.out.stream_item(&OpLine(operation))? {
                return Ok(Some(Stopped::Done));
            }
            self.printed += 1;
            if self.count.is_some_and(|count| self.printed >= count) {
                return Ok(Some(Stopped::Done));
            }
        }
        self.heads.clone_from(&frame.head_operation_ids);
        if self.replay_only && frame.live {
            return Ok(Some(Stopped::Done));
        }
        Ok(None)
    }
}
