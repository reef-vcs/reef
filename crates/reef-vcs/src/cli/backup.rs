//! `reef backup` (R1.18): a stub until then.

use std::path::PathBuf;

use super::CliError;

/// `reef backup`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// The directory to write the backup to.
    dir: PathBuf,
}

pub(crate) fn run(_args: Args) -> Result<(), CliError> {
    Err(CliError::not_implemented("R1.18"))
}
