//! `reef daemon run | start | stop | status`.

use std::path::Path;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reef_daemon::DaemonArgs;
use reef_daemon::client::json::Status;
use reef_daemon::client::{Client, ClientError};
use reef_lib::error::ErrorKind;

use super::{CliError, DEFAULT_TIMEOUT, Global, Out, Output, autostart};

/// How long `stop` waits for the daemon to exit after SIGTERM.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `stop --force` waits after SIGKILL.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// `reef daemon`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    pub(crate) command: DaemonCommand,
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum DaemonCommand {
    /// Run the daemon in this process, in the foreground, until SIGINT or
    /// SIGTERM.
    Run,
    /// Start the daemon in the background unless it is running.
    Start,
    /// Stop the running daemon with SIGTERM.
    Stop {
        /// Send SIGKILL if it has not exited after 15 s.
        #[arg(long)]
        force: bool,
    },
    /// Show the running daemon's version, pid, and counts.
    Status,
}

/// `reef daemon run`: [`reef_daemon::run_main`] in this process, before
/// the CLI starts logging or a runtime.
#[expect(clippy::print_stderr, reason = "the daemon's own failure report")]
pub(crate) fn run_foreground(global: &Global) -> ExitCode {
    let args = DaemonArgs {
        state_dir: global.state_dir.clone(),
        socket: global.socket.clone(),
        config: global.config.clone(),
        foreground: true,
    };
    match reef_daemon::run_main(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error:#}");
            ExitCode::from(reef_daemon::exit_code(&error))
        }
    }
}

pub(crate) async fn run(global: &Global, out: &Out, args: Args) -> Result<(), CliError> {
    match args.command {
        // Dispatched before the runtime starts; see `cli::main`.
        DaemonCommand::Run => Err(CliError::Usage(
            "`daemon run` must be the whole command".to_owned(),
        )),
        DaemonCommand::Start => start(global, out).await,
        DaemonCommand::Stop { force } => stop(global, out, force).await,
        DaemonCommand::Status => {
            let config = global.load_config()?;
            let client = connect_only(&config.paths.socket).await?;
            let status = status_of(&client, global).await?;
            out.object(&status)
        }
    }
}

async fn connect_only(socket: &Path) -> Result<Client, CliError> {
    match Client::connect(socket).await {
        Ok(client) => Ok(client),
        Err(ClientError::Connect { .. }) => Err(CliError::NotRunning {
            socket: socket.to_owned(),
        }),
        Err(error) => Err(CliError::Daemon(error)),
    }
}

async fn status_of(client: &Client, global: &Global) -> Result<Status, CliError> {
    let timeout = global.timeout.unwrap_or(DEFAULT_TIMEOUT);
    match tokio::time::timeout(timeout, client.status()).await {
        Ok(status) => Ok(Status::from(&status.map_err(CliError::Daemon)?)),
        Err(_) => Err(CliError::Timeout { after: timeout }),
    }
}

/// Idempotent: a running daemon is reported, not restarted.
async fn start(global: &Global, out: &Out) -> Result<(), CliError> {
    let config = global.load_config()?;
    let status = match Client::connect(&config.paths.socket).await {
        Ok(client) => {
            let status = status_of(&client, global).await?;
            out.note(&format!("already running (pid {})", status.pid));
            status
        }
        Err(ClientError::Connect { .. }) => {
            let (client, pid) = autostart::start(global, &config).await?;
            out.note(&format!("reef: started daemon (pid {pid})"));
            status_of(&client, global).await?
        }
        Err(error) => return Err(CliError::Daemon(error)),
    };
    if out.mode() == Output::Json {
        out.object(&status)?;
    }
    Ok(())
}

/// Signals only a pid that both the pid file names and the daemon on the
/// socket reports as its own, so a stale pid file never gets an unrelated
/// process killed.
async fn stop(global: &Global, out: &Out, force: bool) -> Result<(), CliError> {
    let config = global.load_config()?;
    let socket = &config.paths.socket;
    let pid_file = &config.paths.pid;
    let recorded = read_pid(pid_file);
    let Ok(client) = Client::connect(socket).await else {
        let detail = recorded.map_or_else(String::new, |pid| {
            format!(
                "; {} names pid {pid}, which was not signalled",
                pid_file.display()
            )
        });
        return Err(CliError::Refused {
            kind: ErrorKind::NotFound,
            message: format!("no daemon answering at {}{detail}", socket.display()),
        });
    };
    let status = status_of(&client, global).await?;
    drop(client);
    if recorded != Some(status.pid) {
        let named =
            recorded.map_or_else(|| "has no pid".to_owned(), |pid| format!("names pid {pid}"));
        return Err(CliError::Refused {
            kind: ErrorKind::Conflict,
            message: format!(
                "the daemon at {} reports pid {}, but {} {named}; not signalling",
                socket.display(),
                status.pid,
                pid_file.display()
            ),
        });
    }
    let pid = Pid::from_raw(i32::try_from(status.pid).map_err(|_| CliError::Refused {
        kind: ErrorKind::Internal,
        message: format!("the daemon reports pid {}, which is not a pid", status.pid),
    })?);
    signal(pid, Signal::SIGTERM)?;
    if !wait_for_exit(pid, STOP_TIMEOUT).await {
        if !force {
            return Err(CliError::Refused {
                kind: ErrorKind::Unavailable,
                message: format!(
                    "the daemon (pid {pid}) did not exit within {}s of SIGTERM; `reef daemon stop --force` kills it",
                    STOP_TIMEOUT.as_secs()
                ),
            });
        }
        signal(pid, Signal::SIGKILL)?;
        if !wait_for_exit(pid, KILL_TIMEOUT).await {
            return Err(CliError::Refused {
                kind: ErrorKind::Internal,
                message: format!("the daemon (pid {pid}) survived SIGKILL"),
            });
        }
    }
    out.done(
        &serde_json::json!({ "stopped": true, "pid": status.pid }),
        &format!("stopped daemon (pid {pid})"),
    )
}

/// The pid in the pid file, if it holds one.
fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn signal(pid: Pid, signal: Signal) -> Result<(), CliError> {
    match kill(pid, signal) {
        // It exited between the status and the signal.
        Ok(()) | Err(Errno::ESRCH) => Ok(()),
        Err(errno) => Err(CliError::Refused {
            kind: if errno == Errno::EPERM {
                ErrorKind::PermissionDenied
            } else {
                ErrorKind::Internal
            },
            message: format!("send {signal} to the daemon (pid {pid}): {errno}"),
        }),
    }
}

/// Whether `pid` is gone within `timeout`.
async fn wait_for_exit(pid: Pid, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if kill(pid, None) == Err(Errno::ESRCH) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
