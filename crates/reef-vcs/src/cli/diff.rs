//! `reef diff` (R2.27): one revision against its parents, or two trees,
//! git-style in text and whole in JSON. Paths given are relative to the
//! current directory inside the scope; paths printed are
//! repository-relative.

use reef_api::v1::{DiffFormat, GetDiffRequest};
use reef_client::json::Diff;

use super::advice::AdviceCtx;
use super::render_vcs::DiffText;
use super::scope::{self, ReadArgs, ScopeArgs};
use super::{CliError, Ctx};

/// `reef diff`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Diff this revision against its parents [default: the workspace's change].
    #[arg(short = 'r', long, value_name = "REV", conflicts_with_all = ["from", "to"])]
    revision: Option<String>,
    /// The old side [default: the new side's parents].
    #[arg(long, value_name = "REV")]
    from: Option<String>,
    /// The new side [default: the workspace's change].
    #[arg(long, value_name = "REV")]
    to: Option<String>,
    /// Each file's added and removed line counts, without hunks.
    #[arg(long, conflicts_with = "summary")]
    stat: bool,
    /// Only which paths changed, and how.
    #[arg(long)]
    summary: bool,
    /// Lines of context around each change [default: 3].
    #[arg(long, value_name = "N")]
    context: Option<u32>,
    /// Only these paths, relative to the current directory [default: every path].
    #[arg(value_name = "PATHS")]
    paths: Vec<String>,
    #[command(flatten)]
    read: ReadArgs,
    #[command(flatten)]
    scope: ScopeArgs,
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let paths = scope::given_paths(&scope, &args.paths)?;
    let advice = AdviceCtx {
        operation_given: args.read.at_op.is_some(),
        snapshots_first: !args.read.no_snapshot,
        ..AdviceCtx::new(&scope)
    };
    // `--from A` alone is A against `@`; `-r B` and `--to B` alone are B
    // against its parents.
    let (from_revision, to_revision) = match (args.revision, args.from, args.to) {
        (Some(revision), _, _) => (String::new(), revision),
        (None, Some(from), to) => (from, to.unwrap_or_else(|| "@".to_owned())),
        (None, None, to) => (String::new(), to.unwrap_or_default()),
    };
    let format = if args.summary {
        DiffFormat::Summary
    } else if args.stat {
        DiffFormat::Stat
    } else {
        DiffFormat::Hunks
    };
    let request = GetDiffRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        from_revision,
        to_revision,
        paths,
        format: format.into(),
        context_lines: args.context,
        at_operation: args.read.at_operation(),
        skip_snapshot: args.read.no_snapshot,
        ..GetDiffRequest::default()
    };
    let response = ctx
        .call(ctx.client.get_diff(request))
        .await
        .map_err(|error| advice.refine(error))?;
    ctx.out.object(&DiffText {
        diff: Diff::from(&response),
        format,
    })
}
