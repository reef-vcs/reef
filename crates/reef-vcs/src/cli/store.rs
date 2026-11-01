//! `reef store build | list` (R1.20): shared dependency stores. `store
//! build`'s deadline is the global `--timeout`, which defaults to 30m for
//! it and is also the build command's timeout on the daemon.

use std::process::ExitCode;
use std::time::Duration;

use reef_api::v1::{BuildStarted, build_store_response, output_line};
use reef_client::ClientError;
use reef_client::json::{BuiltStore, Store};
use reef_error::ErrorKind;

use super::out::shell_line;
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

impl Args {
    /// Whether this is `store build`, which calls an ADMIN RPC.
    pub(crate) fn is_build(&self) -> bool {
        matches!(self.command, StoreCommand::Build { .. })
    }
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
        /// [default: the first bookmark of `main`, origin's HEAD, `master`,
        /// `trunk`, then the same on `origin`; else the root checkout's
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
                    Some(build_store_response::Event::Started(started)) => {
                        ctx.out.note(&started_line(name, &started));
                    }
                    Some(build_store_response::Event::Line(line)) => {
                        let stderr = line.stream() == output_line::Stream::Stderr;
                        ctx.out.passthrough(&line.text, stderr);
                    }
                    Some(build_store_response::Event::Result(result)) => return Ok(Some(result)),
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
        Output::Json | Output::Brief => ctx.out.json_only(&built)?,
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

/// What `store build` prints before the build's output (RX.01, S-03): the
/// command the daemon runs, which comes from the repository's `reef.toml`.
/// Control characters are shown escaped, so a word cannot rewrite the line
/// on a terminal.
fn started_line(name: &str, started: &BuildStarted) -> String {
    let visible = |words: &[String]| {
        let words: Vec<String> = words
            .iter()
            .map(|word| {
                word.chars()
                    .map(|c| {
                        if c.is_control() {
                            c.escape_default().to_string()
                        } else {
                            c.to_string()
                        }
                    })
                    .collect()
            })
            .collect();
        shell_line(&words)
    };
    let rev: String = started.commit_id.chars().take(12).collect();
    let mut line = format!("reef: building {name} at {rev}: {}", visible(&started.argv));
    if !started.env.is_empty() {
        line.push_str("\nreef: env: ");
        line.push_str(&visible(&started.env));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn started_line_quotes_the_argv_and_lists_the_env() {
        let started = BuildStarted {
            argv: vec!["sh".into(), "-c".into(), "npm ci".into()],
            env: vec!["NODE_ENV=production".into()],
            commit_id: "0123456789abcdef".into(),
        };
        assert_eq!(
            started_line("deps", &started),
            "reef: building deps at 0123456789ab: sh -c 'npm ci'\nreef: env: NODE_ENV=production"
        );
        let bare = BuildStarted {
            env: Vec::new(),
            ..started
        };
        assert!(!started_line("deps", &bare).contains("env"));
        let hidden = BuildStarted {
            argv: vec!["sh".into(), "-c".into(), "evil\x1b[2K\rtrue".into()],
            ..bare
        };
        let line = started_line("deps", &hidden);
        assert!(!line.contains('\x1b') && !line.contains('\r'), "{line}");
        assert!(line.contains(r"\u{1b}"), "{line}");
    }
}
