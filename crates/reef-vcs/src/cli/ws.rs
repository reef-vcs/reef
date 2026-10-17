//! `reef ws create | list | inspect | release`, and the subcommands later
//! tickets implement: `move` (R1.06), `run` (R1.21), `gc` (R1.17).

use std::path::PathBuf;
use std::time::Duration;

use reef_daemon::client::default_owner;
use reef_daemon::client::json::{Created, Inspected, Released, Workspace};
use reef_daemon::proto::v0::{CreateRequest, ListWorkspacesRequest, create_request};

use super::repo::missing;
use super::{CliError, Ctx, absolute, repo_arg, ws_move, ws_run};

/// `reef ws`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: WsCommand,
}

#[derive(Debug, clap::Subcommand)]
enum WsCommand {
    /// Create a workspace and print its path.
    Create(CreateArgs),
    /// List active workspaces.
    List {
        /// Only this repository's (id, id prefix, or path).
        #[arg(long, value_name = "REPO")]
        repo: Option<String>,
        /// Include released and reclaimed workspaces.
        #[arg(long)]
        all: bool,
    },
    /// Show one workspace, after a snapshot of its files.
    Inspect {
        /// A workspace id or unique prefix.
        workspace: String,
        /// Report the last snapshot instead of taking one.
        #[arg(long)]
        no_snapshot: bool,
    },
    /// Snapshot a workspace, then remove its directory and end its lease.
    Release {
        /// A workspace id or unique prefix.
        workspace: String,
        /// Skip the final snapshot; unsnapshotted edits are lost.
        #[arg(long)]
        no_snapshot: bool,
    },
    /// Move a workspace to another change.
    Move(ws_move::Args),
    /// Run a command in a new workspace, then release it.
    Run(ws_run::Args),
    /// Delete old operations, unused bases, and released workspace records.
    Gc {
        /// Only this repository (id, id prefix, or path).
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
    /// Start a new change on this revision [default: the `main` bookmark,
    /// else the default workspace's change].
    #[arg(long, value_name = "REV")]
    pub(crate) new_on: Option<String>,
    /// Create the workspace at this directory, which must not exist.
    #[arg(long, value_name = "DIR")]
    pub(crate) path: Option<PathBuf>,
    /// Create the workspace even if a shared store it declares is not built
    /// (R1.20; before it no store is ever missing).
    #[arg(long)]
    pub(crate) skip_missing_stores: bool,
}

impl CreateTarget {
    /// The request these flags describe, owner and lease unset.
    pub(crate) fn request(&self) -> Result<CreateRequest, CliError> {
        let repo = match &self.repo {
            Some(repo) => repo_arg(repo)?,
            None => std::env::current_dir()
                .map_err(|source| CliError::Io {
                    op: "read the current directory",
                    path: PathBuf::from("."),
                    source,
                })?
                .display()
                .to_string(),
        };
        let start = match (&self.change, &self.new_on) {
            (Some(change), _) => Some(create_request::Start::Change(change.clone())),
            (None, Some(new_on)) => Some(create_request::Start::NewOn(new_on.clone())),
            (None, None) => None,
        };
        let path = match &self.path {
            Some(path) => absolute(path)?.display().to_string(),
            None => String::new(),
        };
        Ok(CreateRequest {
            repo,
            start,
            path,
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
    /// The ticket that implements the subcommand, if it is still a stub.
    pub(crate) fn stub(&self) -> Option<&'static str> {
        match self.command {
            WsCommand::Move(_) => Some(ws_move::TICKET),
            WsCommand::Run(_) => Some(ws_run::TICKET),
            WsCommand::Gc { .. } => Some("R1.17"),
            _ => None,
        }
    }
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        WsCommand::Create(args) => create(ctx, args).await,
        WsCommand::List { repo, all } => {
            let states = if all {
                Vec::new()
            } else {
                vec!["active".to_owned(), "releasing".to_owned()]
            };
            let request = ListWorkspacesRequest {
                repo: repo
                    .as_deref()
                    .map(repo_arg)
                    .transpose()?
                    .unwrap_or_default(),
                states,
                owner: String::new(),
            };
            let listed = ctx.call(ctx.client.list_workspaces(request)).await?;
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
        WsCommand::Move(_) | WsCommand::Run(_) | WsCommand::Gc { .. } => Err(
            CliError::not_implemented(args.stub().unwrap_or("a later ticket")),
        ),
    }
}

async fn create(ctx: &Ctx, args: CreateArgs) -> Result<(), CliError> {
    let lease_ms = match args.lease {
        Some(lease) if lease.is_zero() => {
            return Err(CliError::Usage("--lease must be longer than 0s".to_owned()));
        }
        Some(lease) => u64::try_from(lease.as_millis()).unwrap_or(u64::MAX),
        None => 0,
    };
    let request = CreateRequest {
        owner: args.owner.unwrap_or_else(default_owner),
        purpose: args.purpose.unwrap_or_default(),
        lease_ms,
        ..args.target.request()?
    };
    let response = ctx.call(ctx.client.create_workspace(request)).await?;
    let created = Created::from_response(&response).ok_or_else(|| missing("workspace"))?;
    ctx.out.object(&created)
}
