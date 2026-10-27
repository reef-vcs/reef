//! `reef status` (R2.27): the workspace's working copy and what it changes,
//! after a snapshot, and the repository's state; the repository's alone in
//! repository scope. `here` is the cheap "where am I"; this snapshots and
//! lists what changed.

use reef_api::v1::GetStatusRequest;
use reef_client::json::RepoStatus;

use super::advice::AdviceCtx;
use super::scope::{self, ScopeArgs};
use super::{CliError, Ctx};

/// `reef status`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// List at most this many changed files [default: 100].
    #[arg(long, value_name = "N")]
    max_files: Option<u32>,
    /// Report the last snapshot instead of snapshotting the workspace first.
    #[arg(long)]
    no_snapshot: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        snapshots_first: !args.no_snapshot,
        ..AdviceCtx::new(&scope)
    };
    let request = GetStatusRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        skip_snapshot: args.no_snapshot,
        max_files: args
            .max_files
            .map_or(0, |n| i32::try_from(n).unwrap_or(i32::MAX)),
    };
    let response = ctx
        .call(Box::pin(ctx.client.get_status(request)))
        .await
        .map_err(|error| advice.refine(error))?;
    ctx.out.object(&RepoStatus::from(&response))
}
