//! `reef ws move` (R1.06): a stub until then; `cli::ws` refuses it before
//! connecting.

/// `reef ws move`.
#[derive(Debug, clap::Args)]
#[command(group(clap::ArgGroup::new("to").required(true).args(["change", "new_on"])))]
pub(crate) struct Args {
    /// A workspace id or unique prefix.
    workspace: String,
    /// Edit this change.
    #[arg(long, value_name = "REV")]
    change: Option<String>,
    /// Start a new change on this revision.
    #[arg(long, value_name = "REV")]
    new_on: Option<String>,
}

/// The ticket that replaces this stub.
pub(crate) const TICKET: &str = "R1.06";
