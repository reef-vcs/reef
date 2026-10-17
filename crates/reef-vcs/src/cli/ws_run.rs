//! `reef ws run` (R1.21): a stub until then, refused before connecting. The
//! flags are R1.21's, so `--help` shows the final surface.

use std::time::Duration;

use super::ws::CreateTarget;

/// `reef ws run`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(flatten)]
    target: CreateTarget,
    /// What the workspace is for [default: the command line].
    #[arg(long, value_name = "TEXT")]
    purpose: Option<String>,
    /// Who holds it (default `<user>@<host>:<pid>`).
    #[arg(long, value_name = "OWNER")]
    owner: Option<String>,
    /// The lease length [default: the daemon's lease.default].
    #[arg(long, value_name = "DURATION", value_parser = humantime::parse_duration)]
    lease: Option<Duration>,
    /// Run in this existing workspace instead of creating one.
    #[arg(long, value_name = "WS")]
    workspace: Option<String>,
    /// Keep the workspace when the command exits.
    #[arg(long)]
    keep: bool,
    /// Add KEY=VALUE to the command's environment (repeatable).
    #[arg(long, value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// The command and its arguments.
    #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

/// The ticket that replaces this stub.
pub(crate) const TICKET: &str = "R1.21";
