//! `reef diff` (R2.27): one revision against its parents, or two trees,
//! git-style in text and whole in JSON. Paths given are relative to the
//! current directory inside the scope; paths printed are
//! repository-relative. `--symbols` lists each file's changed definitions
//! and their call sites, and `--expand` shows one symbol's lines (R2.15).

use reef_api::v1::{DiffFormat, GetDiffRequest, SymbolSource};
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
    /// Each file's added, removed, changed and moved symbols, with their call sites.
    #[arg(long, group = "symbol_view")]
    symbols: bool,
    /// Only this symbol's lines: a symbol, a suffix such as `Parser#parse`, or a name; repeatable.
    #[arg(long, value_name = "SYMBOL", group = "symbol_view", conflicts_with_all = ["summary", "stat"])]
    expand: Vec<String>,
    /// Call sites listed per symbol, 0 for none [default: 10].
    #[arg(long, value_name = "N", requires = "symbol_view")]
    call_sites: Option<u32>,
    /// Answer from this symbol source for every file.
    #[arg(long, value_name = "SOURCE", value_parser = ["tags", "scip"], requires = "symbol_view")]
    source: Option<String>,
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
    let symbols = args.symbols || !args.expand.is_empty();
    let format = if !args.expand.is_empty() {
        DiffFormat::Hunks
    } else if args.stat {
        DiffFormat::Stat
    } else if args.summary || args.symbols {
        DiffFormat::Summary
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
        symbols: args.symbols,
        max_call_sites: args.call_sites,
        expand_symbols: args.expand.clone(),
        symbol_source: match args.source.as_deref() {
            Some("tags") => SymbolSource::Tags,
            Some("scip") => SymbolSource::Scip,
            _ => SymbolSource::Unspecified,
        }
        .into(),
        ..GetDiffRequest::default()
    };
    let response = ctx
        .call(ctx.client.get_diff(request))
        .await
        .map_err(|error| advice.refine(error))?;
    let diff = Diff::from(&response);
    if symbols {
        notes(ctx, &diff, &args.expand);
    }
    ctx.out.object(&DiffText {
        diff,
        format,
        symbols: args.symbols,
    })
}

/// The notes a symbol diff leaves on stderr: call sites still pending, a
/// diff too large for symbols, and `--expand` entries that matched nothing.
fn notes(ctx: &Ctx, diff: &Diff, expand: &[String]) {
    let Some(summary) = &diff.symbol_summary else {
        return;
    };
    let pending = diff
        .files
        .iter()
        .filter_map(|f| f.symbols.as_ref())
        .flat_map(|s| &s.changes)
        .any(|c| c.call_site_status.as_deref() == Some("index_incomplete"));
    if pending {
        let commit = &diff.to_commit_id;
        let short = commit.get(..12).unwrap_or(commit);
        ctx.out.note(&format!(
            "reef: call sites are pending while {short} is indexed; run reef symbols index -r {commit} to wait, then this command again"
        ));
    }
    if summary.diff_too_large {
        ctx.out
            .note("reef: no symbols: more than 1,000 files differ; name paths to narrow the diff");
    }
    for entry in expand {
        let matched = diff.files.iter().any(|f| {
            f.symbols.as_ref().is_some_and(|s| {
                s.changes.iter().any(|c| {
                    reef_client::symbol_text::matches_expand(
                        entry,
                        &c.symbol,
                        c.old_symbol.as_deref(),
                        &c.name,
                    )
                })
            })
        });
        if !matched {
            ctx.out
                .note(&format!("reef: no changed symbol matches {entry}"));
        }
    }
}
