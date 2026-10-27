//! `reef log` (R2.27): the changes a revset names, newest first, with
//! their graph edges in JSON. No text graph: R2.19 draws it.

use reef_api::v1::ListChangesRequest;
use reef_client::json::LogEntry;

use super::advice::AdviceCtx;
use super::render_vcs::LogLines;
use super::scope::{self, ReadArgs, ScopeArgs};
use super::{CliError, Ctx};

/// `log`'s and `op log`'s default `--limit`.
pub(crate) const LOG_LIMIT: usize = 100;

/// `reef log`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Which changes, as a jj revset [default: `trunk() | working_copies() | mutable()`].
    #[arg(
        short = 'r',
        long = "revisions",
        visible_alias = "revision",
        value_name = "REVSET"
    )]
    revisions: Option<String>,
    /// Show at most this many; 0 for all. A cut is noted on stderr.
    #[arg(long, value_name = "N", default_value_t = LOG_LIMIT)]
    limit: usize,
    #[command(flatten)]
    read: ReadArgs,
    #[command(flatten)]
    scope: ScopeArgs,
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        operation_given: args.read.at_op.is_some(),
        snapshots_first: !args.read.no_snapshot,
        ..AdviceCtx::new(&scope)
    };
    let request = ListChangesRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        revset: args.revisions.unwrap_or_default(),
        at_operation: args.read.at_operation(),
        skip_snapshot: args.read.no_snapshot,
        ..ListChangesRequest::default()
    };
    let limited = ctx
        .call(ctx.client.list_changes_limited(request, args.limit))
        .await
        .map_err(|error| advice.refine(error))?;
    if limited.more {
        ctx.out.note(&format!(
            "reef: showing the first {} changes; --limit 0 shows all",
            limited.items.len()
        ));
    }
    let lines = LogLines {
        entries: limited.items.iter().map(LogEntry::from).collect(),
        workspace: scope.workspace_id(),
    };
    ctx.out.object(&lines)
}
