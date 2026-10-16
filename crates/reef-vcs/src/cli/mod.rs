//! The `reef` command line. R1.04 adds `reef config show`; R1.22
//! restructures the CLI around it.

mod config;

use std::path::PathBuf;

use reef_daemon::config::PathOverrides;

/// `reef`.
#[derive(Debug, clap::Parser)]
#[command(
    name = "reef",
    version,
    about = "Reef: version control built for a world where AI agents write most of the code"
)]
pub(crate) struct Cli {
    /// The daemon's state directory; overrides `REEF_STATE_DIR`.
    #[arg(long, global = true, value_name = "DIR")]
    state_dir: Option<PathBuf>,
    /// The daemon's socket; overrides `REEF_SOCKET`.
    #[arg(long, global = true, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// The daemon's configuration file; overrides `REEF_CONFIG`.
    #[arg(long, global = true, value_name = "FILE")]
    config: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, clap::Subcommand)]
enum Command {
    /// Inspect the daemon's configuration.
    Config(config::ConfigArgs),
}

impl Cli {
    /// Runs the command.
    pub(crate) fn run(self) -> anyhow::Result<()> {
        let overrides = PathOverrides {
            state_dir: self.state_dir,
            socket: self.socket,
            config_file: self.config,
        };
        match self.command {
            Command::Config(args) => args.run(&overrides),
        }
    }
}
