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
use super::{CliError, Ctx, Output, repo_arg};

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
    /// Runs the store's `build` command once, in a scratch workspace on the
    /// revision, and seals what it wrote read-only. Its output is printed as
    /// it runs (all on stderr in JSON mode). Exits with the command's exit
    /// code if it fails. A store already built for the lockfile is not
    /// rebuilt without --force. The command runs for at most --timeout
    /// (default 30m).
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
            let repo = repo
                .as_deref()
                .map(repo_arg)
                .transpose()?
                .unwrap_or_default();
            let stores = ctx.call(ctx.client.list_stores(&repo)).await?;
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
        Some(repo) => repo_arg(&repo)?,
        None => super::current_dir()?.display().to_string(),
    };
    let timeout = ctx.timeout_flag.unwrap_or(BUILD_TIMEOUT);
    let rev = rev.unwrap_or_default();
    let deadline = timeout.saturating_add(BUILD_SLACK);
    let streamed = async {
        let mut events = ctx
            .client
            .build_store(&repo, &name, &rev, timeout, force)
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
    };
    let Some(result) = ctx.call_within(deadline, streamed).await? else {
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
