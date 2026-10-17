//! `reef repo register | list | forget`.

use std::path::PathBuf;

use reef_daemon::client::json::Repository;
use reef_lib::error::ErrorKind;

use super::{CliError, Ctx, absolute, repo_arg};

/// `reef repo`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: RepoCommand,
}

#[derive(Debug, clap::Subcommand)]
enum RepoCommand {
    /// Register the jj repository containing PATH (default: the current
    /// directory).
    Register {
        /// A path inside the repository.
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// List registered repositories.
    List,
    /// Unregister a repository; refused while it has active workspaces.
    Forget {
        /// A repository id, id prefix, or path inside its root.
        repo: String,
    },
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        RepoCommand::Register { path } => {
            let path = absolute(&path)?;
            let response = ctx.call(ctx.client.register_repository(&path)).await?;
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
                });
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
                .call(ctx.client.forget_repository(&repo_arg(&repo)?))
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
    }
}
