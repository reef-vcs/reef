//! `reef ws create | list | inspect | snapshot | renew | release | move | run | provider | gc`.

use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use reef_api::v1::MissingStores;
use reef_api::v1::{
    CreateWorkspaceRequest, CreateWorkspaceResponse, GetWorkspaceRequest, GetWorkspaceResponse,
    ListWorkspacesRequest, WorkspaceState, create_workspace_request,
};
use reef_client::ROOT_CHECKOUT;
use reef_client::brief;
use reef_client::default_owner;
use reef_client::json::{Created, GcReport, Inspected, Lease, Released, Snapshotted, Workspace};
use reef_error::{Advice, ErrorKind};
use serde::Serialize;

use super::advice::AdviceCtx;
use super::changes::{note_divergence, short, workspace_after};
use super::heartbeat::{Heartbeater, heartbeats};
use super::holder::{self, HoldWhile};
use super::render::{GcReports, lease_text, now_ms, span};
use super::repo::missing;
use super::{CliError, Ctx, Global, RepoArg, absolute, provider, scope, ws_move, ws_run};

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
        after_help = "Example: cd \"$(reef ws create -o text --purpose 'fix the parser')\"\nProvenance (R3.04): --model, --task, --instruction-file, --identity, --actor; see `reef provenance declare --help`."
    )]
    Create(CreateArgs),
    /// List active workspaces, newest first, with how many files each changed.
    #[command(visible_alias = "ls")]
    List {
        /// Only this repository's (id, id prefix, or path).
        #[arg(long, value_name = "REPO")]
        repo: Option<String>,
        /// Include released and reclaimed workspaces.
        #[arg(long, conflicts_with_all = ["at_op", "at"])]
        all: bool,
        /// Show at most this many, the newest; 0 for all. A cut is noted on
        /// stderr, so exactly N may mean there are more.
        #[arg(long, value_name = "N", default_value_t = LIST_LIMIT)]
        limit: usize,
        #[command(flatten)]
        at: scope::AtArgs,
    },
    /// Show one workspace, after a snapshot of its files.
    Inspect {
        /// A workspace id, unique prefix, or path inside it.
        workspace: String,
        /// Report the last snapshot instead of taking one.
        #[arg(long)]
        no_snapshot: bool,
        #[command(flatten)]
        at: scope::AtArgs,
    },
    /// Snapshot a workspace's files now, as an operation of its own.
    Snapshot {
        /// A workspace id, unique prefix, or path inside it [default: the one you are in].
        workspace: Option<String>,
        /// The operation's description [default: Reef's own, naming the workspace].
        #[arg(short = 'm', long = "message", value_name = "TEXT")]
        message: Option<String>,
    },
    /// Renew a workspace's lease: what `ws run` does while its command runs.
    #[command(after_help = "Example: reef ws renew   # inside the workspace")]
    Renew {
        /// A workspace id, unique prefix, or path inside it [default: the one you are in].
        workspace: Option<String>,
        /// Renew this lease, failing unless it is the workspace's current one.
        #[arg(long, value_name = "ID")]
        lease_id: Option<String>,
        /// Keep renewing on the daemon's cadence until the lease ends or a signal.
        #[arg(long)]
        follow: bool,
        /// With --follow, stop once this process exits.
        #[arg(long, value_name = "PID", requires = "follow")]
        while_pid: Option<i32>,
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
    #[command(
        after_help = "Example: reef ws run -- cargo test\nProvenance (R3.04): --model, --task, --instruction-file, --identity, --actor; see `reef provenance declare --help`."
    )]
    Run(ws_run::Args),
    /// Create, attach and release workspaces for a program, JSON in and out.
    Provider(provider::Args),
    /// Delete old operations, unused bases, and released workspace records.
    ///
    /// Snapshots every active workspace, then removes what is older than
    /// `gc.undo_window` (7 days) and bases no active workspace uses. Exits
    /// 8, report printed, if the jj step stopped short (as while a `jj`
    /// command holds your checkout). The daemon runs it daily
    /// (`gc.interval`; `0s` is off).
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
    /// Edit this revision: a jj revset naming one commit, such as a change or
    /// commit id prefix, a bookmark, or `trunk()`.
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
    pub(crate) fn request(&self) -> Result<CreateWorkspaceRequest, CliError> {
        let repo = self.repo()?.text();
        let start = match (&self.change, &self.new_on) {
            (Some(change), _) => Some(create_workspace_request::Start::Change(change.clone())),
            (None, Some(new_on)) => Some(create_workspace_request::Start::NewOn(new_on.clone())),
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
        Ok(CreateWorkspaceRequest {
            repo,
            start,
            path,
            missing_stores: missing_stores.into(),
            ..CreateWorkspaceRequest::default()
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
    #[command(flatten)]
    provenance: super::provenance::SessionFlags,
}

impl Args {
    /// Whether this is `ws run`, whose stdout belongs to its command.
    pub(crate) fn is_run(&self) -> bool {
        matches!(self.command, WsCommand::Run(_))
    }

    /// Whether this is `ws provider`, which holds workspaces for a local
    /// process.
    pub(crate) fn is_provider(&self) -> bool {
        matches!(self.command, WsCommand::Provider(_))
    }

    /// Whether this is `ws renew --follow`, which follows a local process
    /// and keeps a local record.
    pub(crate) fn is_follow(&self) -> bool {
        matches!(self.command, WsCommand::Renew { follow: true, .. })
    }

    /// Whether this is `ws gc`, which calls an ADMIN RPC.
    pub(crate) fn is_gc(&self) -> bool {
        matches!(self.command, WsCommand::Gc { .. })
    }
}

pub(crate) async fn run(ctx: &Ctx, global: &Global, args: Args) -> Result<ExitCode, CliError> {
    let done = match args.command {
        WsCommand::Provider(args) => provider::run(ctx, global, args).await,
        WsCommand::Run(args) => return ws_run::run(ctx, args).await,
        WsCommand::Gc { repo, dry_run } => return gc(ctx, repo.as_deref(), dry_run).await,
        WsCommand::Create(args) => create(ctx, args).await,
        WsCommand::List {
            repo,
            all,
            limit,
            at,
        } => list(ctx, repo, all, limit, &at).await,
        WsCommand::Inspect {
            workspace,
            no_snapshot,
            at,
        } => {
            let response = if at.given() {
                inspect_at(ctx, &workspace, &at).await?
            } else {
                ctx.call(ctx.client.inspect_workspace("", &workspace, !no_snapshot))
                    .await?
            };
            let inspected =
                Inspected::from_response(&response).ok_or_else(|| missing("workspace"))?;
            ctx.out.object(&inspected)
        }
        WsCommand::Snapshot { workspace, message } => {
            snapshot(ctx, workspace.as_deref(), message.as_deref()).await
        }
        WsCommand::Renew {
            workspace,
            lease_id,
            follow: true,
            while_pid,
        } => {
            follow(
                ctx,
                global,
                workspace.as_deref(),
                lease_id.as_deref(),
                while_pid,
            )
            .await
        }
        WsCommand::Renew {
            workspace,
            lease_id,
            ..
        } => renew(ctx, workspace.as_deref(), lease_id.as_deref()).await,
        WsCommand::Release {
            workspace,
            no_snapshot,
        } => release(ctx, &workspace, no_snapshot).await,
        WsCommand::Move(args) => ws_move::run(ctx, args).await,
    };
    done.map(|()| ExitCode::SUCCESS)
}

/// `ws inspect` at an operation (R2.12): never snapshots.
async fn inspect_at(
    ctx: &Ctx,
    workspace: &str,
    at: &scope::AtArgs,
) -> Result<GetWorkspaceResponse, CliError> {
    let request = GetWorkspaceRequest {
        repo: String::new(),
        workspace: workspace.to_owned(),
        snapshot_first: false,
        skip_disk_usage: true,
        at_operation: String::new(),
    };
    let (repo_id, at_operation) = if at.at.is_some() {
        let now = ctx.call(ctx.client.get_workspace(request.clone())).await?;
        let ws = now.workspace.ok_or_else(|| missing("workspace"))?;
        let scope = scope::Scope {
            repo_id: ws.repo_id.clone(),
            dir: PathBuf::from(&ws.path),
            workspace: Some(ws),
            from_flags: true,
        };
        (scope.repo_id.clone(), at.at_operation(ctx, &scope).await?)
    } else {
        (String::new(), at.at_op.clone().unwrap_or_default())
    };
    ctx.call(ctx.client.get_workspace(GetWorkspaceRequest {
        repo: repo_id,
        at_operation,
        ..request
    }))
    .await
}

/// The workspace `workspace` names, else the one you are in, as the
/// daemon reads it: a canonical path or an id.
async fn named_or_current(ctx: &Ctx, workspace: Option<&str>) -> Result<String, CliError> {
    match workspace {
        Some(workspace) => scope::workspace_arg(workspace),
        None => match scope::implicit_workspace(ctx).await? {
            Some(ws) => Ok(ws.id),
            None => Err(CliError::Precheck {
                kind: ErrorKind::InvalidArgument,
                reason: reef_api::error::WORKSPACE_REQUIRED,
                message: "not in a workspace".to_owned(),
                advice: Advice::run(
                    "Name the workspace, or run this inside one.",
                    ["reef", "ws", "list"],
                ),
            }),
        },
    }
}

/// What `ws renew` renewed.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[schemars(rename = "cli.Renewed")]
pub(crate) struct Renewed {
    workspace_id: String,
    lease: Lease,
}

/// `reef ws renew` (R2.11): one heartbeat for a lease held outside `ws
/// run`, of the workspace's current lease unless `lease_id` names one. No
/// owner check: renewing does not take the workspace over.
async fn renew(ctx: &Ctx, workspace: Option<&str>, lease_id: Option<&str>) -> Result<(), CliError> {
    let named = named_or_current(ctx, workspace).await?;
    let ws = ctx
        .call(ctx.client.inspect_workspace("", &named, false))
        .await?
        .workspace
        .ok_or_else(|| missing("workspace"))?;
    let lease_id = match lease_id {
        Some(id) => id.to_owned(),
        None => ws
            .lease
            .as_ref()
            .ok_or_else(|| missing("lease"))?
            .id
            .clone(),
    };
    let response = ctx
        .call(ctx.client.renew_lease("", &ws.id, &lease_id))
        .await?;
    let lease = Lease::from(response.lease.as_ref().ok_or_else(|| missing("lease"))?);
    let every = lease
        .heartbeat_every_ms
        .map(|ms| format!(", heartbeat every {}", span(i64::try_from(ms).unwrap_or(0))))
        .unwrap_or_default();
    let note = format!(
        "renewed lease {} of {}: {}{every}",
        lease.id,
        ws.id,
        lease_text(Some(&lease), now_ms())
    );
    ctx.out.done(
        &Renewed {
            workspace_id: ws.id,
            lease,
        },
        &note,
    )
}

/// `reef ws list`.
async fn list(
    ctx: &Ctx,
    repo: Option<String>,
    all: bool,
    limit: usize,
    at: &scope::AtArgs,
) -> Result<(), CliError> {
    let states: Vec<i32> = if all {
        Vec::new()
    } else {
        vec![
            WorkspaceState::Active.into(),
            WorkspaceState::Releasing.into(),
        ]
    };
    let list = |repo| {
        ctx.client.list_workspaces(ListWorkspacesRequest {
            repo,
            states: states.clone(),
            changed_file_counts: true,
            ..ListWorkspacesRequest::default()
        })
    };
    let listed = if at.given() {
        // One repository's, at one of its operations (R2.12).
        let scope = scope::resolve_repo_only(ctx, repo.as_deref()).await?;
        let at_operation = at.at_operation(ctx, &scope).await?;
        let advice = AdviceCtx {
            operation_given: true,
            ..AdviceCtx::new(&scope)
        };
        ctx.call(ctx.client.list_workspaces(ListWorkspacesRequest {
            repo: scope.repo_id.clone(),
            at_operation,
            ..ListWorkspacesRequest::default()
        }))
        .await
        .map_err(|error| advice.refine(error))?
    } else {
        match repo {
            Some(repo) => ctx.call_with_repo(&RepoArg::parse(&repo)?, list).await?,
            None => ctx.call(list(String::new())).await?,
        }
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
    ctx.out.list_with(&listed, brief::workspaces)
}

/// `reef ws release`.
async fn release(ctx: &Ctx, workspace: &str, no_snapshot: bool) -> Result<(), CliError> {
    let response = ctx
        .call(
            ctx.client
                .release_workspace("", workspace, !no_snapshot, false),
        )
        .await?;
    let released = Released::from(&response);
    let note = format!("released {}", released.id);
    ctx.out.done(&released, &note)
}

/// How often a follower checks that its holder still runs.
const HOLDER_CHECK: Duration = Duration::from_secs(2);

/// `reef ws renew --follow` (R2.26): renews the lease on the daemon's
/// cadence until it is gone, the `--while-pid` process exits, or a SIGTERM
/// or SIGINT, recording itself under `<state>/holders/` meanwhile, after
/// its first renewal. Prints nothing on stdout; its notes go to stderr.
async fn follow(
    ctx: &Ctx,
    global: &Global,
    workspace: Option<&str>,
    lease_id: Option<&str>,
    while_pid: Option<i32>,
) -> Result<(), CliError> {
    use tokio::signal::unix::{SignalKind, signal};

    // First, so a signal at any point below ends the follower cleanly.
    let mut terminate = signal(SignalKind::terminate()).map_err(CliError::Signals)?;
    let mut interrupt = signal(SignalKind::interrupt()).map_err(CliError::Signals)?;
    let holder = match while_pid {
        Some(pid) => match holder::resolve(HoldWhile::Pid(pid)) {
            Ok(holder) => Some(holder),
            // Already gone: nothing to hold the lease for.
            Err(_) => return Ok(()),
        },
        None => None,
    };
    let named = named_or_current(ctx, workspace).await?;
    // The first renewal is retried while the daemon is away (a restart,
    // a quiesce): the program that asked for the workspace was already
    // told it is held.
    let mut backoff = Duration::from_secs(1);
    let started = std::time::Instant::now();
    let (ws, renewed) = loop {
        match first_renewal(ctx, &named, lease_id).await {
            Ok(found) => break found,
            Err(error) if !retryable(&error) => return Err(error),
            Err(error) => {
                let holder_gone = holder.as_ref().is_some_and(|holder| !holder::alive(holder));
                let unheld_too_long = holder.is_none() && started.elapsed() > FIRST_RENEWAL_UNHELD;
                if holder_gone {
                    return Ok(());
                }
                if unheld_too_long {
                    return Err(error);
                }
                ctx.out.note(&format!(
                    "reef: cannot renew yet ({}); retrying in {}",
                    reef_error::ErrorChain(&error),
                    humantime::format_duration(backoff)
                ));
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    _ = terminate.recv() => return Ok(()),
                    _ = interrupt.recv() => return Ok(()),
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    };
    let me = holder::resolve(HoldWhile::Pid(
        i32::try_from(std::process::id()).unwrap_or(0),
    ))?;
    let state_dir = global.state_dir()?;
    let record = holder::Record {
        workspace_id: ws.id.clone(),
        lease_id: renewed.id.clone(),
        follower: me,
        holder,
        since: humantime::format_rfc3339_millis(std::time::SystemTime::now()).to_string(),
    };
    holder::write(&state_dir, &record).map_err(|source| CliError::Io {
        op: "write",
        path: state_dir.join("holders"),
        source,
    })?;
    let _record = RecordGuard {
        state_dir,
        workspace_id: ws.id.clone(),
        follower: me,
    };
    let out = ctx.out.clone();
    let heartbeater = Heartbeater::new(
        std::sync::Arc::new(ctx.client.clone()),
        &ws,
        &renewed,
        Box::new(move |text: &str| out.note(text)),
    )?;
    let mut check = tokio::time::interval(HOLDER_CHECK);
    let beats = heartbeats(heartbeater);
    tokio::pin!(beats);
    loop {
        tokio::select! {
            () = &mut beats => break,
            _ = terminate.recv() => break,
            _ = interrupt.recv() => break,
            _ = check.tick() => {
                if holder.as_ref().is_some_and(|holder| !holder::alive(holder)) {
                    break;
                }
            }
        }
    }
    Ok(())
}

/// How long a follower with no holder retries its first renewal.
const FIRST_RENEWAL_UNHELD: Duration = Duration::from_secs(60);

/// The workspace and its lease, renewed once.
async fn first_renewal(
    ctx: &Ctx,
    named: &str,
    lease_id: Option<&str>,
) -> Result<(reef_api::v1::Workspace, reef_api::v1::Lease), CliError> {
    let ws = ctx
        .call(ctx.client.inspect_workspace("", named, false))
        .await?
        .workspace
        .ok_or_else(|| missing("workspace"))?;
    let lease_id = match lease_id {
        Some(id) => id.to_owned(),
        None => ws
            .lease
            .as_ref()
            .ok_or_else(|| missing("lease"))?
            .id
            .clone(),
    };
    let renewed = ctx
        .call(ctx.client.renew_lease("", &ws.id, &lease_id))
        .await?
        .lease
        .ok_or_else(|| missing("lease"))?;
    Ok((ws, renewed))
}

/// Whether a failed first renewal may succeed later: the daemon away, not
/// a lease or workspace that is gone.
fn retryable(error: &CliError) -> bool {
    use reef_error::Classify as _;
    matches!(
        error.kind(),
        ErrorKind::Unavailable | ErrorKind::Internal | ErrorKind::Io
    )
}

/// Removes a follower's record on every way out.
struct RecordGuard {
    state_dir: PathBuf,
    workspace_id: String,
    follower: holder::Holder,
}

impl Drop for RecordGuard {
    fn drop(&mut self) {
        holder::remove(&self.state_dir, &self.workspace_id, &self.follower);
    }
}

/// `reef ws snapshot` (R2.27): the explicit form of what `ws inspect` does
/// on the way to its report, with the workspace read afterwards so a
/// divergent result shows.
async fn snapshot(
    ctx: &Ctx,
    workspace: Option<&str>,
    message: Option<&str>,
) -> Result<(), CliError> {
    let id = named_or_current(ctx, workspace).await?;
    let response = ctx
        .call(
            ctx.client
                .snapshot_workspace("", &id, message.unwrap_or_default()),
        )
        .await?;
    let workspace = workspace_after(ctx, Some(&id)).await;
    let snapshotted = Snapshotted::from_response(&response, workspace);
    let ws = snapshotted
        .workspace
        .as_ref()
        .map_or(id.as_str(), |ws| ws.id.as_str());
    let note = match &snapshotted.operation {
        Some(op) => format!(
            "snapshot {} of {ws}: {}",
            short(&op.id),
            short(&snapshotted.commit_id)
        ),
        None => format!("nothing to snapshot in {ws}"),
    };
    ctx.out.done(&snapshotted, &note)?;
    note_divergence(ctx, snapshotted.workspace.as_ref());
    Ok(())
}

/// `ws list`'s default `--limit`: the one list that grows without bound in
/// Phase 1 (ADR 0025).
const LIST_LIMIT: usize = 100;

/// The `limit` newest of `listed` (all when 0), newest first.
fn newest(mut listed: Vec<reef_api::v1::Workspace>, limit: usize) -> Vec<reef_api::v1::Workspace> {
    // The daemon lists newest first (R2.01).
    if limit > 0 {
        listed.truncate(limit);
    }
    listed
}

/// `ws gc`'s deadline when `--timeout` is not given: GC rebuilds the index
/// and runs `git gc`, which takes minutes on a large repository.
const GC_TIMEOUT: Duration = Duration::from_hours(1);

/// `reef ws gc`: the report, then exit 8 (`Io`) if a repository's jj step,
/// or its evidence and artifacts step (R3.02), stopped short, as the
/// README's exit codes have it; the report is the command's one JSON value
/// either way.
async fn gc(ctx: &Ctx, repo: Option<&str>, dry_run: bool) -> Result<ExitCode, CliError> {
    let deadline = ctx.timeout_flag.unwrap_or(GC_TIMEOUT);
    let gc = |repo: String| async move { ctx.client.collect_garbage(&repo, dry_run).await };
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
    let mut failed = Vec::new();
    for report in &reports.reports {
        for (step, error) in [
            ("jj", report.jj_error.as_ref()),
            ("evidence and artifacts", report.evidence_error.as_ref()),
            ("provenance", report.provenance_error.as_ref()),
        ] {
            if let Some(error) = error {
                failed.push(format!(
                    "error: repo {}: the {step} step of GC stopped short: {error}",
                    report.repo_id
                ));
            }
        }
    }
    for line in &failed {
        ctx.out.error_line(line);
    }
    if failed.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(ErrorKind::Io.exit_code()))
    }
}

/// `--lease` as the request's `lease_length`: unset for the daemon's
/// default.
pub(crate) fn lease_length(
    lease: Option<Duration>,
) -> Result<Option<prost_types::Duration>, CliError> {
    match lease {
        Some(lease) if lease.is_zero() => {
            Err(CliError::Usage("--lease must be longer than 0s".to_owned()))
        }
        Some(lease) => Ok(Some(prost_types::Duration {
            seconds: i64::try_from(lease.as_secs()).unwrap_or(i64::MAX),
            nanos: i32::try_from(lease.subsec_nanos()).unwrap_or(0),
        })),
        None => Ok(None),
    }
}

/// A create still running after this long is explained on stderr (#188).
pub(crate) const CREATE_NOTE_AFTER: Duration = Duration::from_secs(5);
/// The stderr line [`create_noted`] prints after [`CREATE_NOTE_AFTER`].
const CREATE_NOTE: &str = "reef: creating the workspace; a create that builds the base cache (the first in a repository, and on overlay or copy each new tree) checks the whole tree out, which can take minutes on a large one";

/// Awaits `call`, calling `note` once if it has not finished within
/// `after`.
pub(crate) async fn noted<T>(
    call: impl Future<Output = T>,
    after: Duration,
    note: impl FnOnce(),
) -> T {
    tokio::pin!(call);
    tokio::select! {
        result = &mut call => result,
        () = tokio::time::sleep(after) => {
            note();
            call.await
        }
    }
}

/// `ws create`'s and `ws run`'s create: [`Ctx::create_with_repo`], saying
/// on stderr what it waits for once it has run [`CREATE_NOTE_AFTER`]
/// (#188).
pub(crate) async fn create_noted(
    ctx: &Ctx,
    repo: &RepoArg,
    request: CreateWorkspaceRequest,
) -> Result<CreateWorkspaceResponse, CliError> {
    // Boxed: the create's future is large (`clippy::large_futures`).
    noted(
        Box::pin(ctx.create_with_repo(repo, request)),
        CREATE_NOTE_AFTER,
        || {
            ctx.out.note(CREATE_NOTE);
        },
    )
    .await
}

/// [`create_noted`], dropping a provenance declaration that came only from
/// `REEF_MODEL` and `REEF_TASK` when the daemon records no provenance: the
/// environment's defaults never fail a creation (R3.04).
pub(crate) async fn create_declared(
    ctx: &Ctx,
    repo: &RepoArg,
    request: CreateWorkspaceRequest,
    flags: &super::provenance::SessionFlags,
) -> Result<CreateWorkspaceResponse, CliError> {
    let retry = request.provenance.is_some() && flags.only_from_env();
    match create_noted(ctx, repo, request.clone()).await {
        Err(CliError::Daemon(error))
            if retry && error.reason() == reef_api::error::PROVENANCE_DISABLED =>
        {
            let request = CreateWorkspaceRequest {
                provenance: None,
                ..request
            };
            create_noted(ctx, repo, request).await
        }
        created => created,
    }
}

async fn create(ctx: &Ctx, args: CreateArgs) -> Result<(), CliError> {
    let request = CreateWorkspaceRequest {
        owner: args.owner.unwrap_or_else(default_owner),
        purpose: args.purpose.unwrap_or_default(),
        lease_length: lease_length(args.lease)?,
        provenance: args.provenance.declaration()?,
        ..args.target.request()?
    };
    let response = create_declared(ctx, &args.target.repo()?, request, &args.provenance).await?;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// #188 test 4: a call still running after `after` is noted once. The
    /// call finishes only once the note has fired, so the order is forced.
    #[tokio::test]
    async fn noted_notes_a_slow_call_once() {
        let mut count = 0;
        let (noted_tx, noted_rx) = tokio::sync::oneshot::channel();
        let value = noted(
            async {
                noted_rx.await.expect("the note fired");
                7
            },
            Duration::from_millis(1),
            || {
                count += 1;
                noted_tx.send(()).expect("the call is waiting");
            },
        )
        .await;
        assert_eq!(value, 7);
        assert_eq!(count, 1);
    }

    /// #188 test 5: a call that finishes first is not noted.
    #[tokio::test]
    async fn noted_is_silent_for_a_quick_call() {
        let mut count = 0;
        let value = noted(async { 7 }, Duration::from_secs(10), || count += 1).await;
        assert_eq!(value, 7);
        assert_eq!(count, 0);
    }
}
