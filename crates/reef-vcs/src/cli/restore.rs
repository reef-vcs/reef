//! `reef restore` (R1.18): a stub until then. Restore is offline: it never
//! talks to the daemon.

use std::path::PathBuf;

use super::CliError;

/// `reef restore`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The backup directory to restore from.
    dir: PathBuf,
}

pub(crate) fn run(_args: Args) -> Result<(), CliError> {
    Err(CliError::not_implemented("R1.18"))
}
