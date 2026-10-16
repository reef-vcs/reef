//! Entry point for the Reef daemon; the body is [`reef_daemon::run_main`].

use clap::Parser as _;
use reef_daemon::DaemonArgs;

fn main() -> anyhow::Result<()> {
    reef_daemon::run_main(DaemonArgs::parse())
}
