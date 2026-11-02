//! Starting the daemon in the background: on a command that needs it when
//! nothing answers on the socket, and for `reef daemon start`.
//!
//! The program is `reef-daemon` beside the running `reef`, else the first
//! on `PATH` ([`locate`], which `reef daemon run`, `config show` and
//! `restore` use too); `reef` itself contains no daemon (R2.05). It is spawned
//! detached: stdin, stdout, and stderr on `/dev/null` (the daemon logs to
//! `<state>/logs/`), its own process group, and `/` as its working
//! directory so it never pins a workspace directory. Two CLIs starting it
//! at once are settled by the daemon's single-instance lock: the loser
//! exits and both connect to the winner.
//!
//! A daemon holds the lock from before it binds its socket until after it
//! closes it: while it starts up (migrations, reconciling) and while it
//! shuts down, when it refuses connections as it drains (tonic 0.14.7
//! closes the listener first). A daemon started in either window loses the
//! lock and exits (#162). So while `<state>/daemon.lock` is held and the
//! socket refuses, nothing is started until the holder answers or lets the
//! lock go, and a started daemon that exits is checked for the same.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use reef_client::paths::ClientConfig;
use reef_client::{Client, ClientError};
use reef_error::{Advice, ErrorKind};

use super::{CliError, Global, absolute};

/// How long a started daemon gets to answer on its socket.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the socket is still tried after the started daemon exited: long
/// enough for another CLI's daemon, which won the lock, to answer.
const AFTER_EXIT: Duration = Duration::from_secs(1);
/// How often the socket is tried meanwhile.
const POLL: Duration = Duration::from_millis(100);
/// How long a daemon that holds the lock without answering gets to answer
/// or let it go before nothing is started: a shutdown's drain and hooks, as
/// `reef daemon stop` allows them, or a start's migrations.
const HOLDER_WAIT: Duration = Duration::from_secs(15);
/// The daemon binary's name.
pub(crate) const DAEMON_BIN: &str = "reef-daemon";
/// The single-instance lock's name in the state directory (the daemon's
/// `LOCK_FILE`), which its holder writes its pid into.
const LOCK_FILE: &str = "daemon.lock";

/// A daemon that answers, once [`start`] is done.
pub(crate) struct Started {
    /// Connected to it.
    pub(crate) client: Client,
    /// Its pid, as it reports it.
    pub(crate) pid: u32,
    /// Whether this call started it; false when the daemon that held the
    /// lock answered instead.
    pub(crate) spawned: bool,
}

/// Starts a daemon for `config`'s socket and waits until it answers.
pub(crate) async fn start(global: &Global, config: &ClientConfig) -> Result<Started, CliError> {
    start_with(global, config, false).await
}

/// [`start`], with the remote listener on for this run when
/// `serve_remote` (`reef-daemon --remote`, R2.08).
pub(crate) async fn start_with(
    global: &Global,
    config: &ClientConfig,
    serve_remote: bool,
) -> Result<Started, CliError> {
    let program = daemon_binary()?;
    let socket = &config.socket;
    // A daemon that answered while it was waited for is used, not replaced.
    let mut child = if wait_for_holder(config).await? {
        None
    } else {
        let mut command = command(&program, global)?;
        if serve_remote {
            command.arg("--remote");
        }
        let child = command.spawn().map_err(|error| CliError::Autostart {
            reason: format!("run {}: {error}", program.display()),
        })?;
        tracing::debug!(program = %program.display(), pid = child.id(), "daemon spawned");
        Some(child)
    };
    let mut deadline = Instant::now() + START_TIMEOUT;
    let mut exited: Option<ExitStatus> = None;
    loop {
        if let Ok(client) = Client::connect(socket).await {
            let status = tokio::time::timeout(START_TIMEOUT, client.status())
                .await
                .map_err(|_| CliError::Autostart {
                    reason: format!("{} accepted but did not answer", socket.display()),
                })?
                .map_err(|error| match error {
                    // The daemon binary it found is another version: say
                    // which, instead of restarting it in a loop (R2.01).
                    ClientError::Incompatible {
                        daemon_version,
                        daemon_api,
                        ..
                    } => CliError::Autostart {
                        reason: format!(
                            "{} started reef {daemon_version} (API {daemon_api}), not this reef's {} (API v1); install the matching reef-daemon or remove that one",
                            program.display(),
                            reef_client::VERSION
                        ),
                    },
                    error => CliError::Daemon(error),
                })?;
            return Ok(Started {
                client,
                pid: status.pid,
                spawned: child.is_some(),
            });
        }
        if let Some(running) = child.as_mut().filter(|_| exited.is_none()) {
            exited = running.try_wait().ok().flatten();
            if exited.is_some() {
                // Losing the lock exits too: to another CLI's daemon, or
                // to one that was starting before the lock was looked at.
                if wait_for_holder(config).await? {
                    // It answers; the next try connects.
                    child = None;
                    continue;
                }
                // Keep polling briefly: the winner may answer soon.
                deadline = deadline.min(Instant::now() + AFTER_EXIT);
            }
        }
        if Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(POLL).await;
    }
    let reason = match exited {
        Some(status) => format!(
            "{} exited ({status}) before {} answered; run `reef daemon run` to see why",
            program.display(),
            socket.display()
        ),
        None => format!(
            "{} did not answer within {}s{}",
            socket.display(),
            START_TIMEOUT.as_secs(),
            child
                .as_ref()
                .map_or_else(String::new, |child| format!(" (pid {})", child.id()))
        ),
    };
    Err(CliError::Autostart { reason })
}

/// Waits while a daemon holds `config`'s state directory's lock without
/// answering on the socket: one starting up or shutting down (#162).
/// Whether it answered; `false` when the lock is free, or freed meanwhile.
async fn wait_for_holder(config: &ClientConfig) -> Result<bool, CliError> {
    let lock = config.state_dir.join(LOCK_FILE);
    if !lock_held(&lock) {
        return Ok(false);
    }
    tracing::debug!(lock = %lock.display(), "a daemon holds the lock but does not answer; waiting");
    let deadline = Instant::now() + HOLDER_WAIT;
    loop {
        if Client::connect(&config.socket).await.is_ok() {
            return Ok(true);
        }
        if !lock_held(&lock) {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            let holder = std::fs::read_to_string(&lock)
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok());
            let who = holder.map_or_else(
                || "reef-daemon".to_owned(),
                |pid| format!("reef-daemon (pid {pid})"),
            );
            return Err(CliError::Refused {
                kind: ErrorKind::Unavailable,
                message: format!(
                    "{who} holds {} but has not answered on {} within {}s; it may be starting or shutting down",
                    lock.display(),
                    config.socket.display(),
                    HOLDER_WAIT.as_secs()
                ),
                advice: Some(Advice::hint(match holder {
                    Some(pid) => format!("Retry in a moment; if it never answers or exits, stop it with `kill {pid}`."),
                    None => "Retry in a moment; if it never answers or exits, stop the reef-daemon process.".to_owned(),
                })),
            });
        }
        tokio::time::sleep(POLL).await;
    }
}

/// Whether some process holds the single-instance lock at `path`: a shared
/// lock is refused while the daemon holds its exclusive one. The probe's
/// shared lock lasts only this call; a daemon whose start it overlaps loses
/// the lock and exits, and its CLI finds this one's daemon by the same
/// probe.
fn lock_held(path: &Path) -> bool {
    match std::fs::File::open(path) {
        Ok(file) => matches!(
            file.try_lock_shared(),
            Err(std::fs::TryLockError::WouldBlock)
        ),
        Err(_) => false,
    }
}

/// The `reef-daemon` this `reef` runs: beside it, else on `PATH`.
pub(crate) fn daemon_binary() -> Result<PathBuf, CliError> {
    let exe = std::env::current_exe().ok();
    locate(exe.as_deref(), std::env::var_os("PATH").as_deref()).ok_or_else(|| {
        CliError::NoDaemonBinary {
            beside: exe.as_deref().and_then(Path::parent).map(Path::to_owned),
        }
    })
}

/// `reef-daemon` beside `exe`, else on `path_var`.
pub(crate) fn locate(exe: Option<&Path>, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let beside = exe.and_then(Path::parent).map(|dir| dir.join(DAEMON_BIN));
    let on_path = || {
        path_var
            .into_iter()
            .flat_map(std::env::split_paths)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(DAEMON_BIN))
            .find(|candidate| is_executable(candidate))
    };
    beside.filter(|path| is_executable(path)).or_else(on_path)
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The detached command. The path flags and variables the CLI was given
/// are passed on absolute, since the daemon runs in `/`.
fn command(program: &Path, global: &Global) -> Result<Command, CliError> {
    let mut command = Command::new(program);
    path_flags(&mut command, global)?;
    command
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    Ok(command)
}

/// The path flags and variables the CLI was given, onto `command`,
/// absolute.
pub(crate) fn path_flags(command: &mut Command, global: &Global) -> Result<(), CliError> {
    let paths = [
        ("--state-dir", &global.state_dir, "REEF_STATE_DIR"),
        ("--socket", &global.socket, "REEF_SOCKET"),
        ("--config", &global.config, "REEF_CONFIG"),
    ];
    for (flag, given, var) in paths {
        if let Some(path) = given {
            command.arg(flag).arg(absolute(path)?);
        } else if let Some(value) = std::env::var_os(var).filter(|value| !value.is_empty()) {
            command.env(var, absolute(Path::new(&value))?);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn executable(path: &Path) {
        fs::write(path, "#!/bin/sh\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn prefers_beside_then_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let other = tmp.path().join("other");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&other).unwrap();
        let reef = bin.join("reef");
        executable(&reef);
        let path_var = std::env::join_paths([&other]).unwrap();

        // Nothing anywhere: none; reef never runs a daemon itself (R2.05).
        assert_eq!(locate(Some(&reef), Some(&path_var)), None);
        // Not executable on PATH: still none.
        fs::write(other.join(DAEMON_BIN), "").unwrap();
        assert_eq!(locate(Some(&reef), Some(&path_var)), None);
        // On PATH.
        executable(&other.join(DAEMON_BIN));
        assert_eq!(
            locate(Some(&reef), Some(&path_var)),
            Some(other.join(DAEMON_BIN))
        );
        // Beside wins over PATH.
        executable(&bin.join(DAEMON_BIN));
        assert_eq!(
            locate(Some(&reef), Some(&path_var)),
            Some(bin.join(DAEMON_BIN))
        );
        assert_eq!(locate(None, None), None);
    }
}
