//! `reef op log`, `op show` and `undo` (R2.27): the repository's operation
//! log, one operation with its effects, and the revert of one operation.
//! `undo` always names its operation: watchers and other agents record
//! operations every second, so "the last one" is rarely the caller's.

use reef_api::v1::{self, ListOperationsRequest, OperationFilter, UndoOperationRequest};
use reef_client::json::{Operation, Undone};
use reef_error::{Advice, ErrorKind};

use super::advice::AdviceCtx;
use super::changes::{note_divergence, short, workspace_after};
use super::log::LOG_LIMIT;
use super::render_vcs::{OpLines, OpShow};
use super::repo::missing;
use super::scope::{self, ScopeArgs, workspace_arg};
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
}

/// `reef op log`.
#[derive(Debug, clap::Args)]
struct LogArgs {
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
    }
}

async fn log(ctx: &Ctx, args: LogArgs) -> Result<(), CliError> {
    let scope = scope::resolve_repo_only(ctx, args.repo.as_deref()).await?;
    let advice = AdviceCtx::new(&scope);
    let filter = OperationFilter {
        workspaces: args
            .workspace
            .iter()
            .map(|ws| workspace_arg(ws))
            .collect::<Result<_, _>>()?,
        changes: args.change,
        agents: args.by,
        kinds: args
            .kind
            .into_iter()
            .map(|kind| v1::OperationKind::from(kind).into())
            .collect(),
    };
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
