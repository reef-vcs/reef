//! `reef restore` (R1.18, ADR 0020): the daemon's state from a backup.
//! Restore is offline: it never talks to the daemon, and refuses to run
//! while one holds the state directory.

use std::path::PathBuf;

use reef_daemon::error::Advise;
use reef_daemon::maintenance::backup::Manifest;
use reef_daemon::maintenance::restore::{RestoreOptions, restore};
use reef_daemon::model::RepoId;
use reef_lib::error::{Classify, ErrorChain, ErrorKind};
use serde::Serialize;

use super::out::Out;
use super::{CliError, Global, absolute};

/// `reef restore`.
#[derive(Debug, clap::Args)]
#[command(
    after_help = "Stop the daemon first: reef daemon stop && reef restore <dir> --force && reef daemon start"
)]
pub(crate) struct Args {
    /// The backup directory to restore from.
    dir: PathBuf,
    /// The state directory to restore into [default: the daemon's].
    #[arg(long, value_name = "DIR")]
    into: Option<PathBuf>,
    /// Only this repository's jj state (id or id prefix, from the backup);
    /// the database is restored whole either way.
    #[arg(long, value_name = "REPO")]
    repo: Option<String>,
    /// Restore the repository at this directory instead of its registered
    /// root, recreating its working copy if it has none; needs --repo when
    /// the backup has several.
    #[arg(long, value_name = "PATH")]
    root: Option<PathBuf>,
    /// Replace the daemon's current database, and repositories with work
    /// the backup lacks: goes back in time.
    #[arg(long)]
    force: bool,
}

/// What `reef restore -o json` prints.
#[derive(Debug, Serialize)]
struct Restored {
    repos: Vec<RestoredRepo>,
    workspaces_kept: u64,
    leases_extended: u64,
    default_workspace_recreated: bool,
    /// What was replaced, kept under `.replaced-<nanos>` names.
    replaced: Vec<String>,
}

#[derive(Debug, Serialize)]
struct RestoredRepo {
    id: String,
    jj_repo_path: String,
}

pub(crate) fn run(global: &Global, out: &Out, args: &Args) -> Result<(), CliError> {
    let config = global.load_config()?;
    let src = absolute(&args.dir)?;
    let mut opts = RestoreOptions::from_config(&config).map_err(|error| refused(&error))?;
    if let Some(into) = &args.into {
        opts.into = absolute(into)?;
    }
    opts.root = args.root.as_deref().map(absolute).transpose()?;
    opts.force = args.force;
    if let Some(repo) = &args.repo {
        opts.repo = Some(resolve_repo(&src, repo)?);
    }
    let report = restore(&src, &opts).map_err(|error| refused(&error))?;
    let restored = Restored {
        repos: report
            .repos
            .iter()
            .map(|(id, path)| RestoredRepo {
                id: id.as_hex(),
                jj_repo_path: path.display().to_string(),
            })
            .collect(),
        workspaces_kept: report.workspaces_kept,
        leases_extended: report.leases_extended,
        default_workspace_recreated: report.default_workspace_recreated,
        replaced: report
            .replaced
            .iter()
            .map(|path| path.display().to_string())
            .collect(),
    };
    let count = restored.repos.len();
    let note = format!(
        "restored the database and {count} {} from {}; {} active workspaces kept, rematerialized when the daemon starts",
        if count == 1 {
            "repository"
        } else {
            "repositories"
        },
        src.display(),
        restored.workspaces_kept
    );
    out.done(&restored, &note)
}

/// `--repo`: a full id or a prefix of exactly one of the backup's.
fn resolve_repo(src: &std::path::Path, text: &str) -> Result<RepoId, CliError> {
    let manifest = Manifest::read(src).map_err(|error| refused(&error))?;
    let text = text.trim().to_ascii_lowercase();
    let matches: Vec<&str> = manifest
        .repos
        .iter()
        .map(|repo| repo.id.as_str())
        .filter(|id| id.starts_with(&text))
        .collect();
    match matches.as_slice() {
        [id] => id
            .parse()
            .map_err(|_| usage(format!("{id:?} is not a repository id"))),
        [] => Err(usage(format!("the backup has no repository {text:?}"))),
        _ => Err(usage(format!(
            "{text:?} matches {} of the backup's repositories; give more of the id",
            matches.len()
        ))),
    }
}

fn usage(message: String) -> CliError {
    CliError::Refused {
        kind: ErrorKind::InvalidArgument,
        message,
        advice: None,
    }
}

/// The daemon library's error as the command's own, with its kind (the
/// exit code) and advice.
fn refused<E: Classify + Advise + std::error::Error + 'static>(error: &E) -> CliError {
    CliError::Refused {
        kind: error.kind(),
        message: ErrorChain(error).to_string(),
        advice: error.advice(),
    }
}
