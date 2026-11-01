//! `reef daemon run | start | stop | status`.

use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reef_client::json::Status;
use reef_client::{Client, ClientError};
use reef_error::ErrorKind;

use super::{CliError, DEFAULT_TIMEOUT, Global, Out, absolute, autostart};

/// How long `stop` waits for the daemon to exit after SIGTERM.
const STOP_TIMEOUT: Duration = Duration::from_secs(15);
/// The pid file's name in a state directory (`daemon.pid_file` default).
const PID_FILE: &str = "daemon.pid";
/// How long `stop --force` waits after SIGKILL.
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

/// `reef daemon`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    pub(crate) command: DaemonCommand,
}

impl Args {
    /// Whether this is `daemon status`, which a remote daemon answers.
    pub(crate) fn is_status(&self) -> bool {
        matches!(self.command, DaemonCommand::Status)
    }
}

#[derive(Debug, clap::Subcommand)]
pub(crate) enum DaemonCommand {
    /// Run the daemon in this process, in the foreground, until SIGINT or
    /// SIGTERM.
    Run {
        /// Serve the remote listener for this run (`[remote]` in config.toml).
        #[arg(long)]
        serve_remote: bool,
    },
    /// Start the daemon in the background unless it is running.
    Start {
        /// Serve the remote listener for this run, and wait until it listens.
        #[arg(long)]
        serve_remote: bool,
    },
    /// Stop the running daemon with SIGTERM.
    Stop {
        /// Send SIGKILL if it has not exited after 15 s.
        #[arg(long)]
        force: bool,
    },
    /// Show the running daemon's version, pid, and counts.
    Status,
}

/// `reef daemon run`: `exec`s `reef-daemon --foreground` with the path
/// flags as given, before the CLI starts logging or a runtime, so the
/// daemon keeps this process's pid, stdio, environment and exit code
/// (R2.05). Returns only if that fails.
pub(crate) fn run_foreground(global: &Global, out: &Out, serve_remote: bool) -> ExitCode {
    let error = match exec_daemon(global, serve_remote) {
        Ok(never) => match never {},
        Err(error) => error,
    };
    out.error(&error);
    ExitCode::from(error.exit_code())
}

fn exec_daemon(global: &Global, serve_remote: bool) -> Result<std::convert::Infallible, CliError> {
    let program = autostart::daemon_binary()?;
    let mut command = Command::new(&program);
    command.arg("--foreground");
    if serve_remote {
        command.arg("--remote");
    }
    for (flag, value) in [
        ("--state-dir", &global.state_dir),
        ("--socket", &global.socket),
        ("--config", &global.config),
    ] {
        if let Some(value) = value {
            command.arg(flag).arg(absolute(value)?);
        }
    }
    let source = command.exec();
    Err(CliError::Io {
        op: "run",
        path: program,
        source,
    })
}

pub(crate) async fn run(global: &Global, out: &Out, args: Args) -> Result<(), CliError> {
    match args.command {
        // Dispatched before the runtime starts; see `cli::main`.
        DaemonCommand::Run { .. } => Err(CliError::Usage(
            "`daemon run` must be the whole command".to_owned(),
        )),
        DaemonCommand::Start { serve_remote } => start(global, out, serve_remote).await,
        DaemonCommand::Stop { force } => stop(global, out, force).await,
        DaemonCommand::Status => {
            let client = if global.remote.is_some() {
                super::connect(global, out).await?.client
            } else {
                connect_only(global, &global.load_config()?.socket).await?
            };
            let status = status_of(&client, global).await?;
            if status.api != "v1" {
                out.note(&format!(
                    "reef: this daemon is reef {} (API {}) and cannot serve this reef; `reef daemon stop` stops it and the next command starts reef {}",
                    status.version, status.api, reef_client::VERSION
                ));
            }
            out.object(&status)
        }
    }
}

async fn connect_only(global: &Global, socket: &Path) -> Result<Client, CliError> {
    match Client::connect(socket).await {
        Ok(client) => Ok(client),
        Err(ClientError::Connect { .. }) => Err(CliError::NotRunning {
            socket: socket.to_owned(),
            start: global.daemon_start(),
            autostart_off: false,
        }),
        Err(error) => Err(CliError::Daemon(error)),
    }
}

/// The daemon's status: v1's `GetDaemonStatus`, or 0.1's `Daemon/Status`
/// from a 0.1 daemon (R2.01), so `status` and `stop` work against either.
async fn status_of(client: &Client, global: &Global) -> Result<Status, CliError> {
    let timeout = global.timeout.unwrap_or(DEFAULT_TIMEOUT);
    let status = async {
        match client.status().await {
            Ok(status) => Ok(Status::from(&status)),
            Err(ClientError::Incompatible { daemon_api, .. }) if daemon_api == "v0" => {
                Ok(Status::from(&client.v0_status().await?))
            }
            Err(error) => Err(error),
        }
    };
    match tokio::time::timeout(timeout, status).await {
        Ok(status) => status.map_err(CliError::Daemon),
        Err(_) => Err(CliError::Timeout { after: timeout }),
    }
}

/// Whether a daemon reporting `status` is this reef's own: API v1 and the
/// same version.
fn current(status: &Status) -> bool {
    status.api == "v1" && status.version == reef_client::VERSION
}

/// Idempotent: a running daemon is reported, not restarted, unless it
/// speaks another API (a 0.1 daemon, R2.01): an idle one is restarted, and
/// a busy one is reported as an error. A v1 daemon of another version is
/// reported with its version; a later call it cannot serve restarts it then.
async fn start(global: &Global, out: &Out, serve_remote: bool) -> Result<(), CliError> {
    let config = global.load_config()?;
    let status = match Client::connect(&config.socket).await {
        Ok(client) => {
            let status = status_of(&client, global).await?;
            drop(client);
            if serve_remote
                && status
                    .remote
                    .as_ref()
                    .is_none_or(|remote| remote.state == "off")
            {
                return Err(CliError::Refused {
                    kind: ErrorKind::Conflict,
                    message: format!(
                        "the daemon (pid {}) is running without the remote listener",
                        status.pid
                    ),
                    advice: Some(reef_error::Advice::run(
                        "Stop it, then start it with --serve-remote, or set `[remote] enabled = true` in config.toml for every start.",
                        global.daemon_stop(),
                    )),
                });
            }
            if status.api == "v1" {
                if status.version == reef_client::VERSION {
                    out.note(&format!("already running (pid {})", status.pid));
                } else {
                    out.note(&format!(
                        "already running (pid {}, reef {})",
                        status.pid, status.version
                    ));
                }
                status
            } else {
                let incompatible = ClientError::Incompatible {
                    socket: config.socket.clone(),
                    method: "daemon start".to_owned(),
                    daemon_version: status.version.clone(),
                    daemon_api: status.api.clone(),
                    active_workspaces: status.active_workspaces,
                };
                let (client, _) = restart(global, out, incompatible).await?;
                status_of(&client, global).await?
            }
        }
        Err(ClientError::Connect { .. }) => {
            let (client, pid) = autostart::start_with(global, &config, serve_remote).await?;
            out.note(&format!("reef: started daemon (pid {pid})"));
            status_of(&client, global).await?
        }
        Err(error) => return Err(CliError::Daemon(error)),
    };
    let status = if serve_remote {
        wait_listening(global, &config.socket, out).await?
    } else {
        status
    };
    if out.is_json() {
        out.object(&status)?;
    }
    Ok(())
}

/// How long `daemon start --serve-remote` waits for the remote listener.
const LISTEN_TIMEOUT: Duration = Duration::from_secs(5);

/// The daemon's status once its remote listener listens, which `start
/// --serve-remote` notes; an error naming why if it does not within
/// [`LISTEN_TIMEOUT`].
async fn wait_listening(global: &Global, socket: &Path, out: &Out) -> Result<Status, CliError> {
    let deadline = Instant::now() + LISTEN_TIMEOUT;
    let client = connect_only(global, socket).await?;
    loop {
        let status = status_of(&client, global).await?;
        let remote = status.remote.clone();
        match remote {
            Some(remote) if remote.state == "listening" => {
                let fingerprint = remote
                    .certificate
                    .as_ref()
                    .map_or_else(String::new, |cert| format!(", {}", cert.fingerprint));
                out.note(&format!(
                    "reef: remote listener on {}{fingerprint}",
                    remote.address
                ));
                return Ok(status);
            }
            remote if Instant::now() >= deadline => {
                let why = remote
                    .and_then(|remote| remote.error)
                    .unwrap_or_else(|| "it did not start".to_owned());
                return Err(CliError::Refused {
                    kind: ErrorKind::Unavailable,
                    message: format!(
                        "the remote listener is not listening after {}s: {why}",
                        LISTEN_TIMEOUT.as_secs()
                    ),
                    advice: Some(reef_error::Advice::hint(
                        "The daemon keeps retrying; `reef daemon status` shows the listener. Free the port or set `[remote] listen`, then stop and start the daemon.",
                    )),
                });
            }
            _ => tokio::time::sleep(Duration::from_millis(100)).await,
        }
    }
}

/// Stops the daemon and prints that it did.
async fn stop(global: &Global, out: &Out, force: bool) -> Result<(), CliError> {
    let pid = stop_daemon(global, force, None).await?;
    out.done(
        &serde_json::json!({ "stopped": true, "pid": pid }),
        &format!("stopped daemon (pid {pid})"),
    )
}

/// Restarts a daemon of another version that cannot serve this reef
/// (`incompatible`, a [`ClientError::Incompatible`]), if it is idle: stops
/// it as `reef daemon stop` does and starts this reef's, as autostart does,
/// saying so on stderr. A daemon with active workspaces is never signalled;
/// it is reported as [`CliError::OldDaemon`], and so is any daemon under
/// `--no-autostart`.
///
/// Another CLI may be restarting it at the same time, so what the daemon is
/// and whether it is idle are read again just before the stop, and only
/// the pid read then is signalled: a daemon that is already this reef's is
/// used as it is, and one that went away is simply started.
pub(crate) async fn restart(
    global: &Global,
    out: &Out,
    incompatible: ClientError,
) -> Result<(Client, u32), CliError> {
    let ClientError::Incompatible {
        daemon_version,
        daemon_api,
        active_workspaces,
        ..
    } = &incompatible
    else {
        return Err(CliError::Daemon(incompatible));
    };
    let old_daemon = |active_workspaces: u32, error: ClientError| CliError::OldDaemon {
        stop: global.daemon_stop(),
        active_workspaces,
        error: Box::new(error),
    };
    if global.no_autostart || *active_workspaces > 0 {
        return Err(old_daemon(*active_workspaces, incompatible));
    }
    let config = global.load_config()?;
    let socket = &config.socket;
    let mut serve_remote = false;
    match Client::connect(socket).await {
        Ok(client) => {
            let status = status_of(&client, global).await?;
            if current(&status) {
                // Another CLI restarted it meanwhile.
                return Ok((client, status.pid));
            }
            drop(client);
            if status.active_workspaces > 0 {
                return Err(old_daemon(status.active_workspaces, incompatible));
            }
            // A listener `--remote` turned on stays on across the upgrade.
            serve_remote = status
                .remote
                .as_ref()
                .is_some_and(|remote| remote.enabled_by.as_deref() == Some("flag"));
            stop_daemon(global, false, Some(status.pid)).await?;
        }
        // Gone meanwhile: another CLI stopped it.
        Err(ClientError::Connect { .. }) => {}
        Err(error) => return Err(CliError::Daemon(error)),
    }
    let (client, pid) = autostart::start_with(global, &config, serve_remote).await?;
    out.note(&format!(
        "reef: restarted the daemon (reef {daemon_version}, API {daemon_api}, was running)"
    ));
    Ok((client, pid))
}

/// Signals only a pid that both the pid file names and the daemon on the
/// socket reports as its own, so a stale pid file never gets an unrelated
/// process killed, and, when `only` is given, only that pid: a restart
/// never signals a daemon other than the one it checked. The pid stopped.
async fn stop_daemon(global: &Global, force: bool, only: Option<u32>) -> Result<u32, CliError> {
    let config = global.load_config()?;
    let socket = &config.socket;
    let pid_file = &config.pid;
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
            advice: None,
        });
    };
    let status = status_of(&client, global).await?;
    drop(client);
    if let Some(only) = only.filter(|only| *only != status.pid) {
        return Err(CliError::Refused {
            kind: ErrorKind::Conflict,
            message: format!(
                "the daemon at {} changed while restarting it (pid {only}, now {}); not signalling",
                socket.display(),
                status.pid
            ),
            advice: None,
        });
    }
    // The CLI's pid file, or, when `--socket` names a daemon on another
    // state directory, the one in the state directory it reports.
    let theirs = Path::new(&status.state_dir).join(PID_FILE);
    let Some(agreed) = [pid_file.as_path(), theirs.as_path()]
        .into_iter()
        .find(|file| read_pid(file) == Some(status.pid))
    else {
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
            advice: None,
        });
    };
    let gone = Exited {
        socket,
        pid_file: agreed,
    };
    let pid = Pid::from_raw(i32::try_from(status.pid).map_err(|_| CliError::Refused {
        kind: ErrorKind::Internal,
        message: format!("the daemon reports pid {}, which is not a pid", status.pid),
        advice: None,
    })?);
    signal(pid, Signal::SIGTERM)?;
    if !gone.wait(pid, STOP_TIMEOUT).await {
        if !force {
            return Err(CliError::Refused {
                kind: ErrorKind::Unavailable,
                message: format!(
                    "the daemon (pid {pid}) did not exit within {}s of SIGTERM; `reef daemon stop --force` kills it",
                    STOP_TIMEOUT.as_secs()
                ),
                advice: None,
            });
        }
        signal(pid, Signal::SIGKILL)?;
        if !gone.wait(pid, KILL_TIMEOUT).await {
            return Err(CliError::Refused {
                kind: ErrorKind::Internal,
                message: format!("the daemon (pid {pid}) survived SIGKILL"),
                advice: None,
            });
        }
    }
    Ok(status.pid)
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
            advice: None,
        }),
    }
}

/// How `stop` tells that the daemon has exited.
struct Exited<'a> {
    socket: &'a Path,
    pid_file: &'a Path,
}

impl Exited<'_> {
    /// Whether the daemon is gone within `timeout`: its pid no longer
    /// exists, or its pid file is removed and its socket refuses. The
    /// daemon removes both as the last step of shutdown, which covers a
    /// daemon left as an unreaped zombie (a container without an init),
    /// whose pid still exists.
    async fn wait(&self, pid: Pid, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if kill(pid, None) == Err(Errno::ESRCH) {
                return true;
            }
            if !self.pid_file.exists() && Client::connect(self.socket).await.is_err() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}
