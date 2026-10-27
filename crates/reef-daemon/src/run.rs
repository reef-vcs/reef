//! The `reef-daemon` binary's body: [`run_main`] runs the daemon (`reef
//! daemon run` execs the binary to do it, R2.05), and its subcommands are
//! [`crate::offline`]'s. Callable in-process for tests (ADR 0012).

use std::path::PathBuf;

use anyhow::Context as _;
use tokio::signal::unix::{SignalKind, signal};

use reef_lib::error::{Classify, ErrorKind};

use crate::config::{Config, ConfigError, PathOverrides};
use crate::daemon::{Daemon, StartError, log_tightened, private_state_dir};
use crate::lock::LockError;
use crate::logging::{self, LogOptions, LoggingError};
use crate::offline::Offline;

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
    /// A command that needs no running daemon (`config show`, `restore`),
    /// for `reef`; none runs the daemon.
    #[command(subcommand)]
    pub command: Option<Offline>,
}

/// Resolves the configuration ([`Config::load`]), installs logging (stderr
/// and `<state>/logs/daemon.log`), starts the daemon on a new multi-threaded
/// runtime, and runs until SIGINT or SIGTERM, then shuts down.
///
/// Call it before the process starts any thread: it may set
/// `WATCHMAN_SOCK` (R1.14), which is only sound while the process has one.
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
    // the state directory with the default mode, or create it in a state
    // directory that is then refused (RX.01, S-01).
    let tightened = private_state_dir(&config.paths.state_dir).context("start the daemon")?;
    // Before logging and the runtime start their threads.
    #[cfg(feature = "watchman")]
    #[allow(unsafe_code, reason = "no other thread exists yet")]
    // SAFETY: nothing above starts a thread; logging and the runtime below do.
    let socket = unsafe { crate::watcher::watchman::export_socket(config.file.watcher.backend) };
    let _logging = logging::init(&config.log).context("initialize logging")?;
    if let Some(mode) = tightened {
        log_tightened(&config.paths.state_dir, mode);
    }
    #[cfg(feature = "watchman")]
    log_socket_export(&socket);
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

#[cfg(feature = "watchman")]
fn log_socket_export(export: &crate::watcher::watchman::SocketExport) {
    use crate::watcher::watchman::SocketExport;
    match export {
        SocketExport::NotNeeded => {}
        SocketExport::Inherited(socket) => {
            tracing::info!(socket = %socket.to_string_lossy(), "using WATCHMAN_SOCK from the environment");
        }
        SocketExport::Exported(socket) => {
            tracing::info!(socket = %socket.display(), "watchman socket found; WATCHMAN_SOCK set");
        }
        SocketExport::Failed(error) => tracing::warn!(
            error = %error,
            "cannot find the watchman socket; each Watchman snapshot will look for it"
        ),
    }
}

/// The exit code for a failed [`run_main`]: a configuration or start error,
/// or a log directory another user owns, exits with its kind's CLI code (2
/// for a bad file or flag, 7 for a directory another user owns, as `reef`
/// does), anything else 1.
pub fn exit_code(error: &anyhow::Error) -> u8 {
    error
        .chain()
        .find_map(|cause| {
            cause
                .downcast_ref::<ConfigError>()
                .map(Classify::kind)
                .or_else(|| cause.downcast_ref::<StartError>().map(Classify::kind))
                // Only the log directory another user owns (RX.01): bad
                // logging options keep exiting 1.
                .or_else(|| {
                    cause
                        .downcast_ref::<LoggingError>()
                        .filter(|error| matches!(error, LoggingError::LogDirNotOwned { .. }))
                        .map(Classify::kind)
                })
        })
        .map_or(1, ErrorKind::exit_code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_errors_exit_by_kind() {
        let error = anyhow::Error::new(ConfigError::NoHome).context("load");
        assert_eq!(exit_code(&error), ErrorKind::InvalidArgument.exit_code());
        assert_eq!(exit_code(&anyhow::anyhow!("other")), 1);
        let error = anyhow::Error::new(StartError::NotOwned {
            path: "/state".into(),
            uid: 1,
            euid: 0,
            setting: "REEF_STATE_DIR",
        })
        .context("start");
        assert_eq!(exit_code(&error), ErrorKind::PermissionDenied.exit_code());
    }
}
