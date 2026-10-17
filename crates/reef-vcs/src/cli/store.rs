//! `reef store build | list` (R1.20): a stub until then. `store build`'s
//! deadline is the global `--timeout`, which R1.20 defaults to 30m for it.

use super::CliError;

/// `reef store`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: StoreCommand,
}

#[derive(Debug, clap::Subcommand)]
enum StoreCommand {
    /// Build a store declared in reef.toml and seal it.
    Build {
        /// The store's name in reef.toml.
        name: String,
        /// The revision whose reef.toml and lockfile to build from.
        #[arg(long, value_name = "REV")]
        rev: Option<String>,
        /// Rebuild even if a sealed store exists for the lockfile's hash.
        #[arg(long)]
        force: bool,
    },
    /// List built stores.
    List,
}

pub(crate) fn run(_args: Args) -> Result<(), CliError> {
    Err(CliError::not_implemented("R1.20"))
}
