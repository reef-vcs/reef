//! `reef history` (R2.16): which changes touched files or symbols, who
//! wrote them, and what became of them, from the daemon's history index
//! (`QueryChanges`). Paths are relative to the current directory, as
//! `reef diff`'s are. It follows pages up to `--limit`.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reef_api::v1::{self, QueryChangesRequest, QueryChangesResponse};
use reef_client::json::History;

use super::advice::AdviceCtx;
use super::log::LOG_LIMIT;
use super::render_vcs::HistoryText;
use super::scope::{self, workspace_arg};
use super::{CliError, Ctx};

/// `reef history`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Only changes touching these files or directories, relative to the current directory.
    #[arg(value_name = "PATH")]
    paths: Vec<String>,
    /// Only changes touching this symbol (`Class#method()` suffix or name); repeatable.
    #[arg(long, value_name = "SYMBOL")]
    symbol: Vec<String>,
    /// Only symbol changes of this kind; repeatable; needs --symbol.
    #[arg(long, value_enum, value_name = "KIND", requires = "symbol")]
    change_kind: Vec<ChangeKindArg>,
    /// Only changes this agent wrote; repeatable.
    #[arg(long, value_name = "AGENT")]
    by: Vec<String>,
    /// Only changes written in this workspace (id, prefix, or path); repeatable.
    #[arg(long, value_name = "WS")]
    workspace: Vec<String>,
    /// Only this change (hex or reverse-hex prefix); repeatable.
    #[arg(long, value_name = "CHANGE")]
    change: Vec<String>,
    /// Only changes with this outcome; repeatable.
    #[arg(long, value_enum, value_name = "OUTCOME")]
    outcome: Vec<OutcomeArg>,
    /// Only changes written since then: an RFC 3339 time, or a duration ago (7d, 36h).
    #[arg(long, value_name = "WHEN", value_parser = when)]
    since: Option<SystemTime>,
    /// Only changes written before then, as --since.
    #[arg(long, value_name = "WHEN", value_parser = when)]
    until: Option<SystemTime>,
    /// Include changes whose diff is empty.
    #[arg(long)]
    include_empty: bool,
    /// Count changes by this instead of listing them; repeatable.
    #[arg(long, value_enum, value_name = "FIELD")]
    group_by: Vec<GroupArg>,
    /// Show at most this many; 0 for all. A cut is noted on stderr.
    #[arg(long, value_name = "N", default_value_t = LOG_LIMIT)]
    limit: usize,
    /// The repository (id, id prefix, or path) [default: the one you are in].
    #[arg(long, value_name = "REPO")]
    repo: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum ChangeKindArg {
    Added,
    Removed,
    Changed,
    Moved,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum OutcomeArg {
    Open,
    Landed,
    Abandoned,
    Undone,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum GroupArg {
    Agent,
    Outcome,
    Workspace,
}

/// An RFC 3339 time, or a humantime duration before now.
pub(crate) fn when(text: &str) -> Result<SystemTime, String> {
    if let Ok(at) = humantime::parse_rfc3339_weak(text) {
        return Ok(at);
    }
    let ago = humantime::parse_duration(text)
        .map_err(|_| format!("{text:?} is neither an RFC 3339 time nor a duration such as 7d"))?;
    SystemTime::now()
        .checked_sub(ago)
        .ok_or_else(|| format!("{text:?} is too long ago"))
}

fn timestamp(at: SystemTime) -> prost_types::Timestamp {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or(Duration::ZERO);
    prost_types::Timestamp {
        seconds: i64::try_from(since.as_secs()).unwrap_or(i64::MAX),
        nanos: i32::try_from(since.subsec_nanos()).unwrap_or(0),
    }
}

#[expect(clippy::too_many_lines, reason = "the request, then its pages")]
pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    let scope = scope::resolve_repo_only(ctx, args.repo.as_deref()).await?;
    let advice = AdviceCtx::new(&scope);
    // The root (`reef history .` there) is every path.
    let paths: Vec<String> = scope::given_paths(&scope, &args.paths)?
        .into_iter()
        .filter(|path| !path.is_empty())
        .collect();
    let request = QueryChangesRequest {
        repo: scope.repo_id.clone(),
        symbols: args.symbol,
        paths,
        symbol_changes: args
            .change_kind
            .iter()
            .map(|kind| {
                i32::from(match kind {
                    ChangeKindArg::Added => v1::SymbolChangeKind::Added,
                    ChangeKindArg::Removed => v1::SymbolChangeKind::Removed,
                    ChangeKindArg::Changed => v1::SymbolChangeKind::Changed,
                    ChangeKindArg::Moved => v1::SymbolChangeKind::Moved,
                })
            })
            .collect(),
        agents: args.by,
        workspaces: args
            .workspace
            .iter()
            .map(|ws| workspace_arg(ws))
            .collect::<Result<_, _>>()?,
        changes: args.change,
        outcomes: args
            .outcome
            .iter()
            .map(|outcome| {
                i32::from(match outcome {
                    OutcomeArg::Open => v1::ChangeOutcome::Open,
                    OutcomeArg::Landed => v1::ChangeOutcome::Landed,
                    OutcomeArg::Abandoned => v1::ChangeOutcome::Abandoned,
                    OutcomeArg::Undone => v1::ChangeOutcome::Undone,
                })
            })
            .collect(),
        since: args.since.map(timestamp),
        until: args.until.map(timestamp),
        include_empty: args.include_empty,
        group_by: args
            .group_by
            .iter()
            .map(|field| {
                i32::from(match field {
                    GroupArg::Agent => v1::ChangeDimension::Agent,
                    GroupArg::Outcome => v1::ChangeDimension::Outcome,
                    GroupArg::Workspace => v1::ChangeDimension::Workspace,
                })
            })
            .collect(),
        page_size: if args.limit == 0 {
            1000
        } else {
            i32::try_from(args.limit.min(1000)).unwrap_or(1000)
        },
        page_token: String::new(),
    };
    let mut whole: Option<QueryChangesResponse> = None;
    let mut next = request.clone();
    let mut cut = false;
    loop {
        let page = ctx
            .call(ctx.client.query_changes(next.clone()))
            .await
            .map_err(|error| advice.refine(error))?;
        let token = page.next_page_token.clone();
        match &mut whole {
            None => whole = Some(page),
            Some(whole) => {
                whole.changes.extend(page.changes);
                whole.coverage = page.coverage;
            }
        }
        let have = whole.as_ref().map_or(0, |w| w.changes.len());
        if args.limit != 0 && have >= args.limit {
            cut = have > args.limit || !token.is_empty();
            if let Some(whole) = &mut whole {
                whole.changes.truncate(args.limit);
            }
            break;
        }
        if token.is_empty() {
            break;
        }
        next.page_token = token;
    }
    let whole = whole.unwrap_or_default();
    if cut {
        ctx.out.note(&format!(
            "reef: showing the first {} changes; --limit 0 shows all",
            whole.changes.len()
        ));
    }
    ctx.out.object(&HistoryText(History::from(&whole)))
}
