//! `reef rebase` and `reef fetch` (#259, ADR 0071, `docs/rebasing.md`):
//! bring a workspace's change onto the newest trunk, fetched from its git
//! remote first, without jj at the root.

use std::io::{self, Write};

use reef_api::error as reason;
use reef_api::v1::{FetchGitRemoteRequest, RebaseChangeRequest, RebaseMode};
use reef_client::brief;
use reef_client::json::{Fetched, Rebased};

use super::advice::AdviceCtx;
use super::changes::{short, workspace_after};
use super::out::{TextRender, shell_line};
use super::scope::{self, ScopeArgs};
use super::{CliError, Ctx};
use reef_error::Advice;

/// `reef rebase`.
#[derive(Debug, clap::Args)]
pub(crate) struct RebaseArgs {
    /// The change to bring up to date; with it, every change below it that
    /// the destination lacks, and every change above it.
    #[arg(value_name = "REV", default_value = "@")]
    revision: String,
    /// Where to; by default trunk after the fetch, or its remote bookmark
    /// (such as main at origin) when that is ahead.
    #[arg(long, short = 'd', visible_alias = "destination", value_name = "REV")]
    onto: Option<String>,
    /// Move only the change and the changes above it, not those below it
    /// (jj's `-s`).
    #[arg(long, short = 's', conflicts_with = "branch")]
    source: bool,
    /// Move the change with every change below it the destination lacks
    /// (jj's `-b`; the default).
    #[arg(long, short = 'b')]
    branch: bool,
    /// Rebase onto the trunk Reef last saw, without fetching.
    #[arg(long)]
    no_fetch: bool,
    /// Refuse unless the revision still names this commit (a full id).
    #[arg(long, value_name = "ID")]
    expected_commit: Option<String>,
    /// jj's `-r` moves one commit out of its stack: not a way to catch up.
    #[arg(short = 'r', value_name = "REV", hide = true)]
    revisions: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef fetch`.
#[derive(Debug, clap::Args)]
pub(crate) struct FetchArgs {
    /// The git remote [default: trunk's, the only one, else `origin`].
    #[arg(long, value_name = "NAME")]
    git_remote: Option<String>,
    /// A branch to fetch; repeat for several [default: trunk's].
    #[arg(long, value_name = "NAME")]
    branch: Vec<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// Reasons a rebase's fetch fails for: the same rebase without the fetch
/// is the way on.
const FETCH_REASONS: [&str; 10] = [
    reason::GIT_AUTH_FAILED,
    reason::GIT_UNREACHABLE,
    reason::GIT_NOT_FOUND,
    reason::GIT_TOO_OLD,
    reason::GIT_FETCH_FAILED,
    reason::FETCH_TIMEOUT,
    reason::FETCH_IN_PROGRESS,
    reason::FETCH_DISABLED,
    reason::GIT_REMOTE_REQUIRED,
    reason::GIT_REMOTE_NOT_FOUND,
];

/// `reef rebase`.
pub(crate) async fn rebase(ctx: &Ctx, args: RebaseArgs) -> Result<(), CliError> {
    if let Some(revision) = &args.revisions {
        // A usage error, as clap's: no reason.
        return Err(CliError::Precheck {
            kind: reef_error::ErrorKind::InvalidArgument,
            reason: "",
            message: format!(
                "`-r {revision}`: reef rebase moves a change with its stack, not one commit"
            ),
            advice: Advice::run(
                "jj's `-r` takes one commit out of its stack; to bring a change up to date, name it as the positional REV (with `--source` to leave the changes below it).",
                ["reef", "rebase", revision.as_str()],
            ),
        });
    }
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        revision: Some(args.revision.clone()),
        snapshots_first: true,
        ..AdviceCtx::new(&scope)
    };
    let mode = if args.source {
        RebaseMode::Source
    } else {
        RebaseMode::Branch
    };
    let request = RebaseChangeRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        revision: args.revision.clone(),
        onto: args.onto.unwrap_or_default(),
        mode: mode.into(),
        expected_commit_id: args.expected_commit.unwrap_or_default(),
        skip_fetch: args.no_fetch,
    };
    // A fetch waits on the network; the daemon bounds it by `[fetch]
    // timeout`.
    let response = Box::pin(ctx.call_within(FETCH_DEADLINE, ctx.client.rebase_change(request)))
        .await
        .map_err(|error| without_fetch(&advice, error))?;
    let workspace = workspace_after(ctx, scope.workspace_ref()).await;
    let rebased = Rebased::from_response(&response, workspace, shell_line);
    ctx.out.object_with(&rebased, brief::Rebased::from)
}

/// `reef fetch`.
pub(crate) async fn fetch(ctx: &Ctx, args: FetchArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx::new(&scope);
    let request = FetchGitRemoteRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        git_remote: args.git_remote.unwrap_or_default(),
        branches: args.branch,
    };
    let response = Box::pin(ctx.call_within(FETCH_DEADLINE, ctx.client.fetch_git_remote(request)))
        .await
        .map_err(|error| advice.refine(error))?;
    let fetched = Fetched::from_parts(
        &response.fetched.unwrap_or_default(),
        response.operation.as_ref(),
    );
    ctx.out.object(&fetched)
}

/// How long the CLI waits for a fetch: past the daemon's longest `[fetch]
/// timeout` (1 hour), so the daemon's answer always arrives.
const FETCH_DEADLINE: std::time::Duration = std::time::Duration::from_mins(61);

/// A rebase whose fetch failed: the daemon's hint, and `next` the same
/// command with `--no-fetch` (decision 8).
fn without_fetch(advice: &AdviceCtx, error: CliError) -> CliError {
    let CliError::Daemon(daemon) = error else {
        return advice.refine(error);
    };
    if !FETCH_REASONS.contains(&daemon.reason()) {
        return advice.refine(CliError::Daemon(daemon));
    }
    let hint = daemon
        .advice()
        .map(|advice| advice.hint)
        .filter(|hint| !hint.is_empty())
        .unwrap_or_else(|| "The fetch failed.".to_owned());
    let mut next = advice.argv.clone();
    next.push("--no-fetch".to_owned());
    CliError::Advised {
        error: Box::new(daemon),
        advice: Advice {
            hint: format!("{hint} `--no-fetch` rebases onto the trunk Reef last saw instead."),
            next,
        },
    }
}

impl TextRender for Rebased {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        if let Some(fetched) = &self.fetched {
            fetched.render(w)?;
        }
        let change = self
            .change
            .as_ref()
            .map_or_else(String::new, |change| short(&change.change_id_reverse_hex));
        if self.started_new_change {
            writeln!(
                w,
                "its change is already on {} (landed); the workspace is on a new empty change {change} on it",
                self.onto
            )?;
        } else if self.up_to_date {
            writeln!(w, "{change} is already on {}'s head", self.onto)?;
        } else {
            let count = self.rebased_count;
            let changes = if count == 1 { "change" } else { "changes" };
            let workspaces: std::collections::BTreeSet<&String> = self
                .rebased
                .iter()
                .flat_map(|change| &change.workspace_ids)
                .collect();
            let moved = match workspaces.len() {
                0 => String::new(),
                1 => ", 1 workspace moved".to_owned(),
                n => format!(", {n} workspaces moved"),
            };
            writeln!(
                w,
                "rebased {change} onto {} ({count} {changes}{moved})",
                self.onto
            )?;
        }
        for note in &self.notes {
            writeln!(w, "{note}")?;
        }
        for conflict in &self.conflicts {
            let more = conflict.paths_total as usize
                - conflict.paths.len().min(conflict.paths_total as usize);
            let tail = if more > 0 {
                format!(" and {more} more")
            } else {
                String::new()
            };
            writeln!(
                w,
                "conflict in {}: {}{tail}",
                short(&conflict.change_id_reverse_hex),
                conflict.paths.join(", ")
            )?;
        }
        if let Some(next) = &self.next {
            writeln!(w, "next: {next}")?;
        }
        Ok(())
    }
}

impl TextRender for Fetched {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        for branch in &self.branches {
            let line = match (&branch.previous_commit_id, &branch.commit_id) {
                _ if branch.missing => "missing".to_owned(),
                (Some(before), Some(now)) if before == now => "up to date".to_owned(),
                (Some(before), Some(now)) => format!("{} → {}", abbrev(before), abbrev(now)),
                (None, Some(now)) => format!("new at {}", abbrev(now)),
                (_, None) => "missing".to_owned(),
            };
            writeln!(w, "fetched {}/{}: {line}", self.git_remote, branch.branch)?;
        }
        for note in &self.notes {
            writeln!(w, "{note}")?;
        }
        Ok(())
    }
}

fn abbrev(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}
