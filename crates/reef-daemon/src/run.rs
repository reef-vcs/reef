//! The `reef-daemon` binary's body, callable in-process so `reef daemon run`
//! (R1.22) runs the same code.

use std::path::PathBuf;

use anyhow::Context as _;
use tokio::signal::unix::{SignalKind, signal};

use crate::config::{Config, PathOverrides, Paths};
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
    /// The configuration file; overrides `REEF_CONFIG`. Read from R1.04 on.
    #[arg(long, value_name = "FILE")]
    pub config: Option<PathBuf>,
    /// Run in the foreground, the only mode; `reef daemon start` detaches
    /// by spawning the binary.
    #[arg(long)]
    pub foreground: bool,
}

/// Resolves the configuration, installs logging (stderr and
/// `<state>/logs/daemon.log`), starts the daemon on a new multi-threaded
/// runtime, and runs until SIGINT or SIGTERM, then shuts down.
pub fn run_main(args: DaemonArgs) -> anyhow::Result<()> {
    let log = LogOptions::from_env().context("initialize logging")?;
    let paths = Paths::resolve(&PathOverrides {
        state_dir: args.state_dir,
        socket: args.socket,
        config_file: args.config,
    })
    .context("resolve the daemon's paths")?;
    let config = Config::new(paths, log);
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
