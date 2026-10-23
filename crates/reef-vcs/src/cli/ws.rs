//! `reef ws create | list | inspect | release | move | run | gc`.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use reef_daemon::client::default_owner;
use reef_daemon::client::json::{Created, GcReport, Inspected, Released, Workspace};
use reef_daemon::proto::v0::create_request::MissingStores;
use reef_daemon::proto::v0::{CreateRequest, ListWorkspacesRequest, create_request};
use reef_daemon::workspace::ROOT_CHECKOUT;
use reef_lib::error::ErrorKind;

use super::render::GcReports;
use super::repo::missing;
use super::{CliError, Ctx, RepoArg, absolute, ws_move, ws_run};

/// `reef ws`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: WsCommand,
}

#[derive(Debug, clap::Subcommand)]
enum WsCommand {
    /// Create a workspace and print its path.
    #[command(
        visible_alias = "new",
        after_help = "Example: cd \"$(reef ws create -o text --purpose 'fix the parser')\""
    )]
    Create(CreateArgs),
    /// List active workspaces, newest first.
    #[command(visible_alias = "ls")]
    List {
        /// Only this repository's (id, id prefix, or path).
        #[arg(long, value_name = "REPO")]
        repo: Option<String>,
        /// Include released and reclaimed workspaces.
        #[arg(long)]
        all: bool,
        /// Show at most this many, the newest; 0 for all. A cut is noted on
        /// stderr, so exactly N may mean there are more.
        #[arg(long, value_name = "N", default_value_t = LIST_LIMIT)]
        limit: usize,
    },
    /// Show one workspace, after a snapshot of its files.
    Inspect {
        /// A workspace id, unique prefix, or path inside it.
        workspace: String,
        /// Report the last snapshot instead of taking one.
        #[arg(long)]
        no_snapshot: bool,
    },
    /// Snapshot a workspace, then remove its directory and end its lease.
    #[command(visible_alias = "rm")]
    Release {
        /// A workspace id, unique prefix, or path inside it.
        workspace: String,
        /// Skip the final snapshot; unsnapshotted edits are lost.
        #[arg(long)]
        no_snapshot: bool,
    },
    /// Snapshot a workspace, then put another change in its directory.
    ///
    /// Its path and lease stay. On overlay backends the directory is
    /// remounted, so a shell inside it must `cd` in again.
    #[command(after_help = "Example: cd \"$(reef ws move -o text \"$PWD\" --new-on main)\"")]
    Move(ws_move::Args),
    /// Run a command in a new workspace, then release it.
    #[command(after_help = "Example: reef ws run -- cargo test")]
    Run(ws_run::Args),
    /// Delete old operations, unused bases, and released workspace records.
    ///
    /// Removes what is older than `gc.undo_window` (7 days) and bases no
    /// active workspace uses, after snapshotting every active workspace.
    /// `jj` in the repository's own checkout then needs `jj workspace
    /// update-stale`. Exits 8, report printed, if the jj step failed.
    Gc {
        /// Only this repository (id, id prefix, or path) [default: every
        /// registered repository].
        #[arg(long, value_name = "REPO")]
        repo: Option<String>,
        /// Report what would be freed without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
}

/// Where a new workspace comes from; shared by `ws create` and `ws run`.
#[derive(Clone, Debug, clap::Args)]
pub(crate) struct CreateTarget {
    /// The repository (id, id prefix, or path); default: the one containing
    /// the current directory.
    #[arg(long, value_name = "REPO")]
    pub(crate) repo: Option<String>,
    /// Edit this change (a change id prefix, commit id prefix, or bookmark).
    #[arg(long, value_name = "REV", conflicts_with = "new_on")]
    pub(crate) change: Option<String>,
    /// Start a new change on this revision [default: the first bookmark of
    /// `main`, origin's HEAD, `master`, `trunk`, then the same on `origin`;
    /// else the root checkout's change].
    #[arg(long, value_name = "REV")]
    pub(crate) new_on: Option<String>,
    /// Create the workspace at this directory, which must not exist.
    #[arg(long, value_name = "DIR")]
    pub(crate) path: Option<PathBuf>,
    /// Create the workspace even if a shared store it declares is not
    /// built; it is marked `store-missing` [default: fail, naming the `reef
    /// store build` to run].
    #[arg(long)]
    pub(crate) skip_missing_stores: bool,
}

impl CreateTarget {
    /// `--repo`, or the current directory.
    pub(crate) fn repo(&self) -> Result<RepoArg, CliError> {
        match &self.repo {
            Some(repo) => RepoArg::parse(repo),
            None => RepoArg::current(),
        }
    }

    /// The request these flags describe, owner and lease unset.
    pub(crate) fn request(&self) -> Result<CreateRequest, CliError> {
        let repo = self.repo()?.text();
        let start = match (&self.change, &self.new_on) {
            (Some(change), _) => Some(create_request::Start::Change(change.clone())),
            (None, Some(new_on)) => Some(create_request::Start::NewOn(new_on.clone())),
            (None, None) => None,
        };
        let path = match &self.path {
            Some(path) => absolute(path)?.display().to_string(),
            None => String::new(),
        };
        let missing_stores = if self.skip_missing_stores {
            MissingStores::Skip
        } else {
            MissingStores::Fail
        };
        Ok(CreateRequest {
            repo,
            start,
            path,
            missing_stores: missing_stores.into(),
            ..CreateRequest::default()
        })
    }
}

/// `reef ws create`.
#[derive(Debug, clap::Args)]
pub(crate) struct CreateArgs {
    #[command(flatten)]
    target: CreateTarget,
    /// What the workspace is for.
    #[arg(long, value_name = "TEXT")]
    purpose: Option<String>,
    /// Who holds it (default `<user>@<host>`).
    #[arg(long, value_name = "OWNER")]
    owner: Option<String>,
    /// The lease length [default: the daemon's lease.default].
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    lease: Option<Duration>,
}

impl Args {
    /// Whether this is `ws run`, whose stdout belongs to its command.
    pub(crate) fn is_run(&self) -> bool {
        matches!(self.command, WsCommand::Run(_))
    }
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode, CliError> {
    let done = match args.command {
        WsCommand::Run(args) => return ws_run::run(ctx, args).await,
        WsCommand::Gc { repo, dry_run } => return gc(ctx, repo.as_deref(), dry_run).await,
        WsCommand::Create(args) => create(ctx, args).await,
        WsCommand::List { repo, all, limit } => {
            let states = if all {
                Vec::new()
            } else {
                vec!["active".to_owned(), "releasing".to_owned()]
            };
            let list = |repo| {
                ctx.client.list_workspaces(ListWorkspacesRequest {
                    repo,
                    states: states.clone(),
                    owner: String::new(),
                })
            };
            let listed = match repo {
                Some(repo) => ctx.call_with_repo(&RepoArg::parse(&repo)?, list).await?,
                None => ctx.call(list(String::new())).await?,
            };
            let total = listed.len();
            let listed = newest(listed, limit);
            if listed.len() < total {
                ctx.out.note(&format!(
                    "reef: showing the newest {} of {total} workspaces; --limit 0 shows all",
                    listed.len()
                ));
            }
            let listed: Vec<Workspace> = listed.iter().map(Workspace::from).collect();
            ctx.out.list(&listed)
        }
        WsCommand::Inspect {
            workspace,
            no_snapshot,
        } => {
            let response = ctx
                .call(ctx.client.inspect_workspace("", &workspace, !no_snapshot))
                .await?;
            let inspected =
                Inspected::from_response(&response).ok_or_else(|| missing("workspace"))?;
            ctx.out.object(&inspected)
        }
        WsCommand::Release {
            workspace,
            no_snapshot,
        } => {
            let response = ctx
                .call(
                    ctx.client
                        .release_workspace("", &workspace, !no_snapshot, false),
                )
                .await?;
            let released = Released::from(&response);
            let note = format!("released {}", released.id);
            ctx.out.done(&released, &note)
        }
        WsCommand::Move(args) => ws_move::run(ctx, args).await,
    };
    done.map(|()| ExitCode::SUCCESS)
}

/// `ws list`'s default `--limit`: the one list that grows without bound in
/// Phase 1 (ADR 0025).
const LIST_LIMIT: usize = 100;

/// The `limit` newest of `listed` (all when 0), newest first.
fn newest(
    mut listed: Vec<reef_daemon::proto::v0::Workspace>,
    limit: usize,
) -> Vec<reef_daemon::proto::v0::Workspace> {
    // The daemon lists oldest first.
    listed.reverse();
    if limit > 0 {
        listed.truncate(limit);
    }
    listed
}

/// `ws gc`'s deadline when `--timeout` is not given: GC rebuilds the index
/// and runs `git gc`, which takes minutes on a large repository.
const GC_TIMEOUT: Duration = Duration::from_hours(1);

/// `reef ws gc`: the report, then exit 8 (`Io`) if a repository's jj step
/// stopped short, as the README's exit codes have it; the report is the
/// command's one JSON value either way.
async fn gc(ctx: &Ctx, repo: Option<&str>, dry_run: bool) -> Result<ExitCode, CliError> {
    let deadline = ctx.timeout_flag.unwrap_or(GC_TIMEOUT);
    let gc = |repo: String| async move { ctx.client.gc(&repo, dry_run).await };
    let response = match repo {
        Some(repo) => {
            ctx.call_with_repo_within(deadline, &RepoArg::parse(repo)?, gc)
                .await?
        }
        None => ctx.call_within(deadline, gc(String::new())).await?,
    };
    let reports = GcReports {
        reports: response.reports.iter().map(GcReport::from).collect(),
        dry_run,
    };
    ctx.out.object(&reports)?;
    let failed: Vec<_> = reports
        .reports
        .iter()
        .filter_map(|report| Some((&report.repo_id, report.jj_error.as_ref()?)))
        .collect();
    for (repo, error) in &failed {
        ctx.out.error_line(&format!(
            "error: repo {repo}: the jj step of GC stopped short: {error}"
        ));
    }
    if failed.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(ErrorKind::Io.exit_code()))
    }
}

/// `--lease` as the request's `lease_ms`: 0 for the daemon's default.
pub(crate) fn lease_millis(lease: Option<Duration>) -> Result<u64, CliError> {
    match lease {
        Some(lease) if lease.is_zero() => {
            Err(CliError::Usage("--lease must be longer than 0s".to_owned()))
        }
        Some(lease) => Ok(u64::try_from(lease.as_millis()).unwrap_or(u64::MAX)),
        None => Ok(0),
    }
}

async fn create(ctx: &Ctx, args: CreateArgs) -> Result<(), CliError> {
    let request = CreateRequest {
        owner: args.owner.unwrap_or_else(default_owner),
        purpose: args.purpose.unwrap_or_default(),
        lease_ms: lease_millis(args.lease)?,
        ..args.target.request()?
    };
    let response = ctx
        .call_with_repo(&args.target.repo()?, |repo| {
            ctx.client.create_workspace(CreateRequest {
                repo,
                ..request.clone()
            })
        })
        .await?;
    let created = Created::from_response(&response).ok_or_else(|| missing("workspace"))?;
    note_default_start(ctx, &response.start);
    ctx.out.object(&created)
}

/// The stderr line when a create without a start revision found no
/// bookmark to start on (R1.31); shared with `ws run`.
pub(crate) fn note_default_start(ctx: &Ctx, start: &str) {
    if start == ROOT_CHECKOUT {
        ctx.out.note(
            "reef: no main, master or trunk bookmark; starting on the root checkout's change (pass --new-on <branch>)",
        );
    }
}
