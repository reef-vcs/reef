//! Starting the daemon in the background: on a command that needs it when
//! nothing answers on the socket, and for `reef daemon start`.
//!
//! The program is `reef-daemon` beside the running `reef`, else the first
//! on `PATH`, else `reef daemon run` (this binary, which runs the same
//! code), so a `reef` installed alone still starts a daemon. It is spawned
//! detached: stdin, stdout, and stderr on `/dev/null` (the daemon logs to
//! `<state>/logs/`), its own process group, and `/` as its working
//! directory so it never pins a workspace directory. Two CLIs starting it
//! at once are settled by the daemon's single-instance lock: the loser
//! exits and both connect to the winner.

use std::ffi::{OsStr, OsString};
use std::os::unix::fs::PermissionsExt as _;
use std::os::unix::process::CommandExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;

use super::{CliError, Global, absolute};

/// How long a started daemon gets to answer on its socket.
const START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long the socket is still tried after the started daemon exited: long
/// enough for another CLI's daemon, which won the lock, to answer.
const AFTER_EXIT: Duration = Duration::from_secs(1);
/// How often the socket is tried meanwhile.
const POLL: Duration = Duration::from_millis(100);
/// The daemon binary's name.
const DAEMON_BIN: &str = "reef-daemon";

/// What to run to start a daemon.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Program {
    pub(crate) path: PathBuf,
    /// `["daemon", "run"]` when the program is `reef` itself.
    pub(crate) args: Vec<OsString>,
}

/// Starts a daemon for `config`'s socket and waits until it answers.
/// Returns a connected client and the daemon's pid as it reports it.
pub(crate) async fn start(global: &Global, config: &Config) -> Result<(Client, u32), CliError> {
    let exe = std::env::current_exe().ok();
    let program = locate(exe.as_deref(), std::env::var_os("PATH").as_deref()).ok_or_else(|| {
        CliError::Autostart {
            reason: format!("cannot find {DAEMON_BIN} or the reef binary itself"),
        }
    })?;
    let mut command = command(&program, global)?;
    let mut child = command.spawn().map_err(|error| CliError::Autostart {
        reason: format!("run {}: {error}", program.path.display()),
    })?;
    tracing::debug!(program = %program.path.display(), pid = child.id(), "daemon spawned");
    let socket = &config.paths.socket;
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
                            program.path.display(),
                            reef_daemon::client::VERSION
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
            program.path.display(),
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

/// `reef-daemon` beside `exe`, else on `path_var`, else `exe daemon run`.
pub(crate) fn locate(exe: Option<&Path>, path_var: Option<&OsStr>) -> Option<Program> {
    let beside = exe.and_then(Path::parent).map(|dir| dir.join(DAEMON_BIN));
    let on_path = || {
        path_var
            .into_iter()
            .flat_map(std::env::split_paths)
            .filter(|dir| dir.is_absolute())
            .map(|dir| dir.join(DAEMON_BIN))
            .find(|candidate| is_executable(candidate))
    };
    if let Some(path) = beside.filter(|path| is_executable(path)).or_else(on_path) {
        return Some(Program {
            path,
            args: Vec::new(),
        });
    }
    exe.map(|exe| Program {
        path: exe.to_owned(),
        args: vec!["daemon".into(), "run".into()],
    })
}

fn is_executable(path: &Path) -> bool {
    path.metadata()
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

/// The detached command. The path flags and variables the CLI was given
/// are passed on absolute, since the daemon runs in `/`.
fn command(program: &Program, global: &Global) -> Result<Command, CliError> {
    let mut command = Command::new(&program.path);
    command.args(&program.args);
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
    command
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0);
    Ok(command)
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
    fn prefers_beside_then_path_then_self() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join("bin");
        let other = tmp.path().join("other");
        fs::create_dir_all(&bin).unwrap();
        fs::create_dir_all(&other).unwrap();
        let reef = bin.join("reef");
        executable(&reef);
        let path_var = std::env::join_paths([&other]).unwrap();

        // Nothing anywhere: reef itself.
        assert_eq!(
            locate(Some(&reef), Some(&path_var)),
            Some(Program {
                path: reef.clone(),
                args: vec!["daemon".into(), "run".into()],
            })
        );
        // Not executable on PATH: still reef itself.
        fs::write(other.join(DAEMON_BIN), "").unwrap();
        assert_eq!(locate(Some(&reef), Some(&path_var)).unwrap().path, reef);
        // On PATH.
        executable(&other.join(DAEMON_BIN));
        assert_eq!(
            locate(Some(&reef), Some(&path_var)).unwrap().path,
            other.join(DAEMON_BIN)
        );
        // Beside wins over PATH.
        executable(&bin.join(DAEMON_BIN));
        let found = locate(Some(&reef), Some(&path_var)).unwrap();
        assert_eq!(found.path, bin.join(DAEMON_BIN));
        assert_eq!(found.args, Vec::<OsString>::new());
        assert_eq!(locate(None, None), None);
    }
}
