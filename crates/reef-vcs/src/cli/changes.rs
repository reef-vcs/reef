//! `reef show`, `describe` and `abandon` (R2.27), and what every write run
//! in a workspace does afterwards: read the workspace once more and note a
//! divergent result (ADR 0018), which the write's own answer does not
//! show for the caller's workspace.

use reef_api::v1::{
    AbandonChangeRequest, DescribeChangeRequest, DiffFormat, GetChangeRequest, GetDiffRequest,
    GetWorkspaceRequest,
};
use reef_client::json::{Abandoned, ChangeSummary, Described, Workspace, reverse_hex};

use super::advice::AdviceCtx;
use super::out::shell_line;
use super::scope::{self, ReadArgs, ScopeArgs, divergent_log};
use super::{CliError, Ctx};

/// Change ids in notes, by their reverse hex; commits by hex.
const ID_WIDTH: usize = 12;

/// `reef show`.
#[derive(Debug, clap::Args)]
pub(crate) struct ShowArgs {
    /// The revision.
    #[arg(value_name = "REV", default_value = "@")]
    revision: String,
    #[command(flatten)]
    read: ReadArgs,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef describe`.
#[derive(Debug, clap::Args)]
pub(crate) struct DescribeArgs {
    /// The revision.
    #[arg(value_name = "REV", default_value = "@")]
    revision: String,
    /// The description; several are joined by blank lines, and '' clears it.
    #[arg(short = 'm', long = "message", value_name = "TEXT", required = true)]
    message: Vec<String>,
    /// Refuse unless the revision still names this commit (a full id).
    #[arg(long, value_name = "ID")]
    expected_commit: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef abandon`.
#[derive(Debug, clap::Args)]
pub(crate) struct AbandonArgs {
    /// The revision: a change or commit id, or a revset naming one commit.
    #[arg(value_name = "REV")]
    revision: String,
    /// Refuse unless the revision still names this commit (a full id).
    #[arg(long, value_name = "ID")]
    expected_commit: Option<String>,
    #[command(flatten)]
    scope: ScopeArgs,
}

pub(crate) async fn show(ctx: &Ctx, args: ShowArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        operation_given: args.read.at.given(),
        snapshots_first: !args.read.no_snapshot,
        ..AdviceCtx::new(&scope)
    };
    let summary = async {
        let change = ctx
            .call(ctx.client.get_change(GetChangeRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                revision: args.revision.clone(),
                at_operation: args.read.at_operation(ctx, &scope).await?,
                skip_snapshot: args.read.no_snapshot,
            }))
            .await?;
        let commit = change
            .change
            .as_ref()
            .map(|change| change.commit_id.clone())
            .unwrap_or_default();
        // The same view as the change, without a second snapshot.
        let diff = ctx
            .call(ctx.client.get_diff(GetDiffRequest {
                repo: scope.repo_id.clone(),
                workspace: scope.workspace_id(),
                to_revision: commit,
                format: DiffFormat::Summary.into(),
                at_operation: change.at_operation.clone(),
                skip_snapshot: true,
                ..GetDiffRequest::default()
            }))
            .await?;
        Ok::<_, CliError>(ChangeSummary::from_responses(&change, &diff))
    }
    .await
    .map_err(|error| advice.refine(error))?;
    ctx.out.object(&summary)
}

pub(crate) async fn describe(ctx: &Ctx, args: DescribeArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let revision = args.revision;
    let advice = AdviceCtx {
        revision: Some(revision.clone()),
        ..AdviceCtx::new(&scope)
    };
    let response = ctx
        .call(ctx.client.describe_change(DescribeChangeRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision,
            description: join_paragraphs(&args.message),
            expected_commit_id: args.expected_commit.unwrap_or_default(),
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let workspace = workspace_after(ctx, scope.workspace_ref()).await;
    let described = Described::from_response(&response, workspace);
    let change = described.change.as_ref();
    let note = match (&described.operation, change) {
        (None, Some(change)) => format!(
            "{} already has that description",
            short(&change.change_id_reverse_hex)
        ),
        (_, Some(change)) => format!(
            "described {} as {}",
            short(&change.change_id_reverse_hex),
            short(&change.commit_id)
        ),
        (_, None) => "described".to_owned(),
    };
    ctx.out.done(&described, &note)?;
    note_divergence(ctx, described.workspace.as_ref());
    Ok(())
}

pub(crate) async fn abandon(ctx: &Ctx, args: AbandonArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let advice = AdviceCtx {
        revision: Some(args.revision.clone()),
        ..AdviceCtx::new(&scope)
    };
    let response = ctx
        .call(ctx.client.abandon_change(AbandonChangeRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision: args.revision.clone(),
            expected_commit_id: args.expected_commit.unwrap_or_default(),
        }))
        .await
        .map_err(|error| advice.refine(error))?;
    let workspace = workspace_after(ctx, scope.workspace_ref()).await;
    let abandoned = Abandoned::from_response(&response, workspace);
    let note = format!("abandoned {}", short(&abandoned.abandoned_commit_id));
    ctx.out.done(&abandoned, &note)?;
    note_divergence(ctx, abandoned.workspace.as_ref());
    Ok(())
}

/// `-m` values as one description, as `jj describe` joins them: each
/// paragraph ending in a newline, separated by a blank line.
fn join_paragraphs(paragraphs: &[String]) -> String {
    paragraphs
        .iter()
        .map(|paragraph| {
            let mut text = paragraph.clone();
            if !text.is_empty() && !text.ends_with('\n') {
                text.push('\n');
            }
            text
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn short(id: &str) -> String {
    id.chars().take(ID_WIDTH).collect()
}

/// Workspace `id` after a write, read without a snapshot; `None` without
/// one (repository scope), or with a warning when the read fails (the
/// write succeeded either way).
pub(crate) async fn workspace_after(ctx: &Ctx, id: Option<&str>) -> Option<Workspace> {
    let id = id?;
    let read = ctx
        .call(ctx.client.get_workspace(GetWorkspaceRequest {
            repo: String::new(),
            workspace: id.to_owned(),
            snapshot_first: false,
            skip_disk_usage: true,
            at_operation: String::new(),
        }))
        .await;
    match read {
        Ok(response) => response.workspace.as_ref().map(Workspace::from),
        Err(error) => {
            ctx.out.note(&format!(
                "reef: could not read workspace {id} after the write: {error}"
            ));
            None
        }
    }
}

/// The stderr note when a write left `workspace` divergent, naming how to
/// see and resolve it.
pub(crate) fn note_divergence(ctx: &Ctx, workspace: Option<&Workspace>) {
    let Some(ws) = workspace.filter(|ws| ws.divergent) else {
        return;
    };
    ctx.out.note(&format!(
        "reef: workspace {} is divergent: its change {} has more than one commit. See them with: {}; abandon the one to drop with reef abandon <commit id>.",
        ws.id,
        short(&reverse_hex(&ws.change_id)),
        shell_line(&divergent_log(&ws.id, &ws.change_id)),
    ));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_join_as_jj_joins_them() {
        let join = |words: &[&str]| {
            join_paragraphs(&words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>())
        };
        assert_eq!(join(&["fix parser"]), "fix parser\n");
        assert_eq!(join(&["fix parser", "body"]), "fix parser\n\nbody\n");
        assert_eq!(join(&["done\n"]), "done\n");
        assert_eq!(join(&[""]), "");
    }
}
