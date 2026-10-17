//! The `reef-daemon` binary's body, callable in-process so `reef daemon run`
//! (R1.22) runs the same code.

use std::path::PathBuf;

use anyhow::Context as _;
use tokio::signal::unix::{SignalKind, signal};

use reef_lib::error::Classify as _;

use crate::config::{Config, ConfigError, PathOverrides};
use crate::daemon::{Daemon, StartError, create_private_dir};
use crate::lock::LockError;
use crate::logging::{self, LogOptions};

/// The binary's command line.
#[derive(Clone, Debug, Default, clap::Parser)]
#[command(name = "reef-daemon", version, about = "The Reef daemon")]
pub struct DaemonArgs {
    /// The state directory; overrides `REEF_STATE_DIR`.
    #[arg(long, value_name = "DIR")]
    pub state_dir: Option<PathBuf>,
    /// The socket to listen on; overrides `REEF_SOCKET`.
    #[arg(long, value_name = "PATH")]
    pub socket: Option<PathBuf>,
    /// The configuration file; overrides `REEF_CONFIG`.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Run in the foreground, the only mode; `reef daemon start` detaches
    /// by spawning the binary.
    #[arg(long)]
    pub foreground: bool,
}

/// Resolves the configuration ([`Config::load`]), installs logging (stderr
/// and `<state>/logs/daemon.log`), starts the daemon on a new multi-threaded
/// runtime, and runs until SIGINT or SIGTERM, then shuts down.
pub fn run_main(args: DaemonArgs) -> anyhow::Result<()> {
    let env_log = LogOptions::from_env().context("initialize logging")?;
    let mut config = Config::load(&PathOverrides {
        state_dir: args.state_dir,
        socket: args.socket,
        config_file: args.config,
    })
    .context("load the daemon's configuration")?;
    config.log.filter = env_log.filter;
    config.log.format = env_log.format;
    // Before logging, which creates `<state>/logs` and would otherwise create
    // the state directory with the default mode.
    create_private_dir(&config.paths.state_dir).context("start the daemon")?;
    let _logging = logging::init(&config.log).context("initialize logging")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_name("reef-daemon-worker")
        .build()
        .context("start the async runtime")?;
    runtime.block_on(serve(config))
}

async fn serve(config: Config) -> anyhow::Result<()> {
    // Installed before the socket exists, so a client that sees the socket
    // can always stop the daemon cleanly.
    let mut terminate = signal(SignalKind::terminate()).context("install the SIGTERM handler")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("install the SIGINT handler")?;
    let daemon = match Daemon::start(config).await {
        Ok(daemon) => daemon,
        // One line naming the holder, not a chain.
        Err(StartError::Lock(LockError::Held { path, pid })) => {
            let holder = pid.map_or_else(
                || "another reef-daemon".to_owned(),
                |pid| format!("another reef-daemon (pid {pid})"),
            );
            anyhow::bail!("{holder} holds {}", path.display());
        }
        Err(error) => return Err(error).context("start the daemon"),
    };
    let signal = tokio::select! {
        _ = terminate.recv() => "SIGTERM",
        _ = interrupt.recv() => "SIGINT",
    };
    tracing::info!(signal, "signal received");
    daemon.shutdown();
    daemon.join().await.context("run the daemon")
}

/// The exit code for a failed [`run_main`]: a configuration error exits with
/// its kind's CLI code (2 for a bad file or flag, as `reef` does), anything
/// else 1.
pub fn exit_code(error: &anyhow::Error) -> u8 {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<ConfigError>())
        .map_or(1, |config| config.kind().exit_code())
}

#[cfg(test)]
mod tests {
    use reef_lib::error::ErrorKind;

    use super::*;

    #[test]
    fn config_errors_exit_by_kind() {
        let error = anyhow::Error::new(ConfigError::NoHome).context("load");
        assert_eq!(exit_code(&error), ErrorKind::InvalidArgument.exit_code());
        assert_eq!(exit_code(&anyhow::anyhow!("other")), 1);
    }
}
