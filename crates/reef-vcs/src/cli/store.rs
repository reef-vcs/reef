//! `reef store build | list` (R1.20): shared dependency stores. `store
//! build`'s deadline is the global `--timeout`, which defaults to 30m for
//! it and is also the build command's timeout on the daemon.

use std::process::ExitCode;
use std::time::Duration;

use reef_daemon::client::ClientError;
use reef_daemon::client::json::{BuiltStore, Store};
use reef_daemon::proto::v0::{build_event, output_line};
use reef_lib::error::ErrorKind;

use super::repo::missing;
use super::{CliError, Ctx, Output, RepoArg};

/// `store build`'s timeout when `--timeout` is not given.
const BUILD_TIMEOUT: Duration = Duration::from_mins(30);

/// How long the CLI waits past the build's own timeout: the daemon first
/// creates the scratch workspace (a full copy of the tree), and afterwards
/// kills the build, releases the workspace, and seals or moves the
/// directory aside before it answers.
const BUILD_SLACK: Duration = Duration::from_mins(10);

/// `reef store`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: StoreCommand,
}

#[derive(Debug, clap::Subcommand)]
enum StoreCommand {
    /// Build a store declared in reef.toml and seal it.
    ///
    /// Runs the store's `build` once in a scratch workspace on the revision
    /// and seals what it wrote. Its output streams (to stderr in JSON mode);
    /// a failure exits with its code. A built store is kept unless --force;
    /// --timeout (default 30m) bounds the command.
    Build {
        /// The store's name in reef.toml.
        name: String,
        /// The repository (id, id prefix, or path); default: the one
        /// containing the current directory.
        #[arg(long, value_name = "REPO")]
        repo: Option<String>,
        /// The revision whose reef.toml and lockfile to build from
        /// [default: the `main` bookmark, else the default workspace's
        /// change].
        #[arg(long, value_name = "REV")]
        rev: Option<String>,
        /// Rebuild even if a sealed store exists for the lockfile's hash.
        #[arg(long)]
        force: bool,
    },
    /// List built stores.
    #[command(visible_alias = "ls")]
    List {
        /// Only the stores this repository built or mounts (id, id prefix,
        /// or path) [default: every store].
        #[arg(long, value_name = "REPO")]
        repo: Option<String>,
    },
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode, CliError> {
    match args.command {
        StoreCommand::Build {
            name,
            repo,
            rev,
            force,
        } => build(ctx, name, repo, rev, force).await,
        StoreCommand::List { repo } => {
            let list = |repo: String| async move { ctx.client.list_stores(&repo).await };
            let stores = match repo {
                Some(repo) => ctx.call_with_repo(&RepoArg::parse(&repo)?, list).await?,
                None => ctx.call(list(String::new())).await?,
            };
            let stores: Vec<Store> = stores.iter().map(Store::from).collect();
            ctx.out.list(&stores)?;
            Ok(ExitCode::SUCCESS)
        }
    }
}

async fn build(
    ctx: &Ctx,
    name: String,
    repo: Option<String>,
    rev: Option<String>,
    force: bool,
) -> Result<ExitCode, CliError> {
    let repo = match repo {
        Some(repo) => RepoArg::parse(&repo)?,
        None => RepoArg::current()?,
    };
    let timeout = ctx.timeout_flag.unwrap_or(BUILD_TIMEOUT);
    let rev = rev.unwrap_or_default();
    let deadline = timeout.saturating_add(BUILD_SLACK);
    // An unregistered repository fails before any output, so a retry after
    // registering it never repeats a line.
    let streamed = |repo: String| {
        let (name, rev) = (&name, &rev);
        async move {
            let mut events = ctx
                .client
                .build_store(&repo, name, rev, timeout, force)
                .await?;
            while let Some(event) = events.message().await.map_err(ClientError::from)? {
                match event.event {
                    Some(build_event::Event::Line(line)) => {
                        let stderr = line.stream() == output_line::Stream::Stderr;
                        ctx.out.passthrough(&line.text, stderr);
                    }
                    Some(build_event::Event::Done(result)) => return Ok(Some(result)),
                    None => {}
                }
            }
            Ok(None)
        }
    };
    let Some(result) = ctx.call_with_repo_within(deadline, &repo, streamed).await? else {
        return Err(missing("build result"));
    };
    let built = BuiltStore::from(&result);
    let short: String = built.lockfile_hash.chars().take(12).collect();
    if built.sealed {
        let note = if built.already_built {
            format!(
                "store {} {short} is already built at {}",
                built.name, built.path
            )
        } else {
            format!("built store {} {short} at {}", built.name, built.path)
        };
        ctx.out.done(&built, &note)?;
        return Ok(ExitCode::SUCCESS);
    }
    match ctx.out.mode() {
        Output::Json => ctx.out.json_only(&built)?,
        Output::Text => ctx.out.error_line(&format!(
            "error: store {} {short} build exited with {}; nothing sealed; what it wrote is kept at {}",
            built.name, built.exit_code, built.path
        )),
    }
    let code = u8::try_from(built.exit_code)
        .ok()
        .filter(|code| *code != 0)
        .unwrap_or_else(|| ErrorKind::Internal.exit_code());
    Ok(ExitCode::from(code))
}
