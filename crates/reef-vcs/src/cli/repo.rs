//! `reef repo register | list | forget`.

use std::path::PathBuf;

use reef_api::json::Repository;
use reef_api::proto::v1::register_request::IfNotJj;
use reef_lib::error::ErrorKind;

use super::{CliError, Ctx, REGISTER_TIMEOUT, RepoArg, absolute};

/// `reef repo`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: RepoCommand,
}

#[derive(Debug, clap::Subcommand)]
enum RepoCommand {
    /// Register the repository containing PATH (default: the current directory).
    ///
    /// A git checkout without jj gets jj alongside git first: a `.jj`
    /// directory and a `/.jj/` line in `.git/info/exclude`.
    Register {
        /// A path inside the repository.
        #[arg(default_value = ".")]
        path: PathBuf,
        /// Refuse a repository that is not a jj repository yet instead of
        /// initializing jj alongside git.
        #[arg(long)]
        no_init: bool,
    },
    /// List registered repositories.
    #[command(visible_alias = "ls")]
    List,
    /// Unregister a repository; refused while it has active workspaces.
    Forget {
        /// A repository id, id prefix, or path inside its root.
        repo: String,
    },
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        RepoCommand::Register { path, no_init } => {
            let path = absolute(&path)?;
            let if_not_jj = if no_init {
                IfNotJj::Fail
            } else {
                IfNotJj::InitAlongsideGit
            };
            let deadline = ctx.timeout_flag.unwrap_or(REGISTER_TIMEOUT);
            let response = ctx
                .call_within(deadline, ctx.client.register_repository(&path, if_not_jj))
                .await?;
            let repo = response
                .repository
                .as_ref()
                .map(Repository::from)
                .ok_or_else(|| missing("repository"))?;
            if response.already_registered {
                return Err(CliError::Refused {
                    kind: ErrorKind::AlreadyExists,
                    message: format!(
                        "repository {} is already registered as {}",
                        repo.root, repo.id
                    ),
                    advice: None,
                });
            }
            if response.initialized_jj {
                ctx.out.note(&format!(
                    "reef: initialized jj alongside git in {}",
                    repo.root
                ));
            }
            ctx.out.object(&repo)
        }
        RepoCommand::List => {
            let repos = ctx.call(ctx.client.list_repositories()).await?;
            let repos: Vec<Repository> = repos.iter().map(Repository::from).collect();
            ctx.out.list(&repos)
        }
        RepoCommand::Forget { repo } => {
            let response = ctx
                .call(ctx.client.forget_repository(&RepoArg::parse(&repo)?.text()))
                .await?;
            let repo = response
                .repository
                .as_ref()
                .map(Repository::from)
                .ok_or_else(|| missing("repository"))?;
            let note = format!("forgot {}", repo.id);
            ctx.out.done(&repo, &note)
        }
    }
}

/// A response without a field the daemon always sets.
pub(crate) fn missing(field: &str) -> CliError {
    CliError::Refused {
        kind: ErrorKind::Internal,
        message: format!("the daemon's answer has no {field}"),
        advice: None,
    }
}
