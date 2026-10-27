//! `reef restore` (R1.18, ADR 0020): the daemon's state from a backup.
//! Restore is offline: it never talks to the daemon, and refuses to run
//! while one holds the state directory. The daemon's binary restores
//! (`reef-daemon restore`, R2.05); this prints what it reported.

use std::ffi::OsString;
use std::path::PathBuf;

use reef_client::json::Restored;

use super::out::Out;
use super::{CliError, Global, absolute, helper};

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

pub(crate) fn run(global: &Global, out: &Out, args: &Args) -> Result<(), CliError> {
    let src = absolute(&args.dir)?;
    let mut command: Vec<OsString> = vec!["restore".into(), src.clone().into()];
    if let Some(into) = &args.into {
        command.extend(["--into".into(), absolute(into)?.into()]);
    }
    if let Some(repo) = &args.repo {
        // One argument, so a value starting with `-` is not read as a flag.
        command.push(format!("--repo={repo}").into());
    }
    if let Some(root) = &args.root {
        command.extend(["--root".into(), absolute(root)?.into()]);
    }
    if args.force {
        command.push("--force".into());
    }
    let bytes = helper::run(global, &command)?;
    let restored: Restored = serde_json::from_slice(&bytes).map_err(|error| CliError::Refused {
        kind: reef_error::ErrorKind::Internal,
        message: format!("read reef-daemon restore's report: {error}"),
        advice: None,
    })?;
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
