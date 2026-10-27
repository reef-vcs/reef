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

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use reef_client::paths::ClientConfig;
use reef_client::{Client, ClientError};

use super::{CliError, Global, absolute};

/// How long a started daemon gets to answer on its socket.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the socket is still tried after the started daemon exited: long
/// enough for another CLI's daemon, which won the lock, to answer.
const AFTER_EXIT: Duration = Duration::from_secs(1);
/// How often the socket is tried meanwhile.
const POLL: Duration = Duration::from_millis(100);
/// The daemon binary's name.
pub(crate) const DAEMON_BIN: &str = "reef-daemon";

/// Starts a daemon for `config`'s socket and waits until it answers.
/// Returns a connected client and the daemon's pid as it reports it.
pub(crate) async fn start(
    global: &Global,
    config: &ClientConfig,
) -> Result<(Client, u32), CliError> {
    let program = daemon_binary()?;
    let mut command = command(&program, global)?;
    let mut child = command.spawn().map_err(|error| CliError::Autostart {
        reason: format!("run {}: {error}", program.display()),
    })?;
    tracing::debug!(program = %program.display(), pid = child.id(), "daemon spawned");
    let socket = &config.socket;
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
            return Ok((client, status.pid));
        }
        if exited.is_none() {
            // Keep polling briefly after an exit: losing the lock to another
            // CLI's daemon also exits, and that daemon answers soon.
            exited = child.try_wait().ok().flatten();
            if exited.is_some() {
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
            "{} did not answer within {}s (pid {})",
            socket.display(),
            START_TIMEOUT.as_secs(),
            child.id()
        ),
    };
    Err(CliError::Autostart { reason })
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
