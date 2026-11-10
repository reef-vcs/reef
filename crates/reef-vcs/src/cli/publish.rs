//! `reef publish` (#258, ADR 0070, `docs/publishing.md`): push a change,
//! and every ancestor the git remote's branches lack, to a review branch,
//! then print the command that opens the pull request.

use std::io::{self, Write};

use reef_api::v1::PublishChangeRequest;
use reef_client::brief;
use reef_client::json::Published;

use super::advice::AdviceCtx;
use super::changes::{join_paragraphs, short, workspace_after};
use super::out::{TextRender, shell_line};
use super::scope::{self, ScopeArgs};
use super::{CliError, Ctx};

/// `reef publish`.
#[derive(Debug, clap::Args)]
#[expect(clippy::struct_excessive_bools, reason = "clap flags")]
pub(crate) struct PublishArgs {
    /// The change to publish.
    #[arg(value_name = "REV", default_value = "@")]
    revision: String,
    /// The branch on the remote [default: `reef/` and the change id's first
    /// 12 letters].
    #[arg(long, value_name = "NAME")]
    branch: Option<String>,
    /// The git remote [default: the only one, else `origin`].
    #[arg(long, value_name = "NAME")]
    git_remote: Option<String>,
    /// Describe the change first; several are joined by blank lines.
    #[arg(short = 'm', long = "message", value_name = "TEXT")]
    message: Vec<String>,
    /// Add a `Signed-off-by:` trailer for the daemon's `[identity]` first.
    #[arg(long)]
    signoff: bool,
    /// Replace a branch holding another change's commit, or move a local
    /// bookmark of its name. Never past the lease, never trunk.
    #[arg(long)]
    force: bool,
    /// Plan and check only: push and record nothing.
    #[arg(long)]
    dry_run: bool,
    /// Refuse unless the revision still names this commit (a full id).
    #[arg(long, value_name = "ID")]
    expected_commit: Option<String>,
    /// Do not snapshot the workspace first.
    #[arg(long)]
    no_snapshot: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef publish`.
pub(crate) async fn publish(ctx: &Ctx, args: PublishArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        revision: Some(args.revision.clone()),
        snapshots_first: !args.no_snapshot,
        ..AdviceCtx::new(&scope)
    };
    let request = PublishChangeRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        revision: args.revision.clone(),
        git_remote: args.git_remote.unwrap_or_default(),
        branch: args.branch.unwrap_or_default(),
        expected_commit_id: args.expected_commit.unwrap_or_default(),
        description: join_paragraphs(&args.message),
        signoff: args.signoff,
        force: args.force,
        dry_run: args.dry_run,
        skip_snapshot: args.no_snapshot,
    };
    // A push waits on the network; the daemon bounds it by `[publish]
    // timeout`, so the client's own deadline does not.
    let response = Box::pin(ctx.call_within(PUBLISH_DEADLINE, ctx.client.publish_change(request)))
        .await
        .map_err(|error| advice.refine(error))?;
    let workspace = workspace_after(ctx, scope.workspace_ref()).await;
    let published = Published::from_response(&response, workspace, shell_line);
    ctx.out.object_with(&published, brief::Published::from)
}

/// How long the CLI waits for a publish: past the daemon's longest
/// `[publish] timeout` (1 hour), so the daemon's answer always arrives.
pub(crate) const PUBLISH_DEADLINE: std::time::Duration = std::time::Duration::from_mins(61);

impl TextRender for Published {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let publication = &self.publication;
        let change = self.change.as_ref().map_or_else(
            || short(&publication.change_id),
            |change| short(&change.change_id_reverse_hex),
        );
        let count = publication.commit_count;
        let commits = if count == 1 { "commit" } else { "commits" };
        let verb = if publication.dry_run {
            "would publish"
        } else {
            "published"
        };
        writeln!(
            w,
            "{verb} {change} to {}/{} ({}, {count} {commits})",
            publication.git_remote,
            publication.branch,
            publication.update.as_deref().unwrap_or("unknown"),
        )?;
        if publication.dry_run {
            for id in &publication.change_ids {
                writeln!(w, "  {}", short(&reef_client::json::reverse_hex(id)))?;
            }
        }
        for note in &self.notes {
            writeln!(w, "{note}")?;
        }
        let hint = publication.pull_request.as_ref();
        if let Some(url) = hint
            .map(|hint| hint.url.as_str())
            .filter(|url| !url.is_empty())
        {
            writeln!(w, "pull request: {url}")?;
        }
        if let Some(next) = &self.next {
            writeln!(w, "next: {next}")?;
        } else if hint.is_none_or(|hint| hint.url.is_empty()) {
            for line in &publication.remote_messages {
                writeln!(w, "remote: {line}")?;
            }
        }
        Ok(())
    }
}
