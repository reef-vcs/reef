//! How a command fails. Every variant has an [`ErrorKind`], which is the
//! exit code (README, CLI: `InvalidArgument` 2, `NotFound` 3,
//! `AlreadyExists` 4, `Conflict` 5, `Unavailable` 6, `PermissionDenied` 7,
//! `Io` 8, `Unsupported` 9, `Internal` 1) and the `kind` of the JSON error
//! object, and may have an [`Advice`]: the `hint` and `next` of ADR 0025.
//! A daemon error's `reason` (R2.01) is the JSON error object's `reason`.

use std::io;
use std::path::PathBuf;
use std::process::ExitStatus;
use std::time::Duration;

use reef_client::ClientError;
use reef_client::logging::LoggingError;
use reef_client::paths::PathsError;
use reef_error::{Advice, Classify, ErrorKind, classify_by_source};

/// A failed command.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    /// The daemon answered with an error, or the connection failed. The
    /// daemon's message is already its whole chain.
    #[error(transparent)]
    Daemon(ClientError),
    /// The running daemon is another version and cannot serve this reef,
    /// and was not restarted: it has active workspaces, or autostart is off
    /// (R2.01).
    #[error("{error}")]
    OldDaemon {
        /// [`ClientError::Incompatible`], boxed: it holds the status.
        error: Box<ClientError>,
        /// `reef daemon stop`, with the global flags that chose the socket.
        stop: Vec<String>,
        /// The daemon's active workspaces.
        active_workspaces: u32,
    },
    /// Nothing answers on the socket and autostart is off.
    #[error("daemon is not running at {}", socket.display())]
    NotRunning {
        /// The socket.
        socket: PathBuf,
        /// The `reef daemon start` that starts it, with the global flags
        /// that chose the socket.
        start: Vec<String>,
        /// Whether `--no-autostart` is why it was not started; `daemon
        /// status` and `stop` never start one.
        autostart_off: bool,
    },
    /// Starting the daemon failed.
    #[error("could not start the daemon: {reason}")]
    Autostart {
        /// What went wrong.
        reason: String,
    },
    /// The daemon did not answer within `--timeout`.
    #[error(
        "the daemon did not answer within {} (--timeout); it may still finish the request",
        humantime::format_duration(*after)
    )]
    Timeout {
        /// The deadline.
        after: Duration,
    },
    /// The configuration did not load.
    #[error("load the daemon's configuration")]
    Config(#[source] PathsError),
    /// No `reef-daemon` beside `reef` or on `PATH`: `reef` contains no
    /// daemon (R2.05).
    #[error(
        "no reef-daemon beside {} or on PATH",
        beside.as_deref().map_or_else(|| "reef".into(), |dir| dir.display().to_string())
    )]
    NoDaemonBinary {
        /// The directory `reef` runs from.
        beside: Option<PathBuf>,
    },
    /// A `reef-daemon` offline command (`config show`, `restore`) failed
    /// and printed its error object.
    #[error("{message}")]
    Helper {
        /// The kind, and so the exit code.
        kind: ErrorKind,
        /// The error's reason, when it has one.
        reason: String,
        /// Its whole chain.
        message: String,
        /// How to fix it.
        advice: Option<Advice>,
    },
    /// A `reef-daemon` offline command failed without printing an error
    /// object.
    #[error("{} failed ({status}) without reporting why", program.display())]
    HelperFailed {
        /// The program.
        program: PathBuf,
        /// How it exited.
        status: ExitStatus,
    },
    /// Logging could not be set up.
    #[error("initialize logging")]
    Logging(#[source] LoggingError),
    /// The async runtime could not start.
    #[error("start the async runtime")]
    Runtime(#[source] io::Error),
    /// `ws run` could not install its signal handlers.
    #[error("install signal handlers")]
    Signals(#[source] io::Error),
    /// A command refused with a kind of its own, such as `repo register` on
    /// a registered repository (`AlreadyExists`).
    #[error("{message}")]
    Refused {
        /// The kind, and so the exit code.
        kind: ErrorKind,
        /// The message.
        message: String,
        /// How to fix it, if the command knows.
        advice: Option<Advice>,
    },
    /// The command line is valid to clap but not to the command.
    #[error("{0}")]
    Usage(String),
    /// A local file operation failed.
    #[error("{op} {}", path.display())]
    Io {
        /// What was being done, such as `read`.
        op: &'static str,
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// Writing the command's output failed.
    #[error("write to stdout")]
    Stdout(#[source] io::Error),
}

impl CliError {
    /// How to fix the failure, when that is known: the daemon's advice for
    /// its own errors, the command's for the rest.
    pub(crate) fn advice(&self) -> Option<Advice> {
        match self {
            Self::Daemon(ClientError::InvalidAgent(_)) => Some(Advice::hint(format!(
                "An agent name is visible ASCII, at most {} bytes (--agent or REEF_AGENT).",
                reef_api::AGENT_MAX_BYTES
            ))),
            Self::Daemon(error) => error.advice(),
            Self::NotRunning {
                start,
                autostart_off,
                ..
            } => Some(Advice {
                hint: if *autostart_off {
                    "Start the daemon, or drop --no-autostart to let any command start it."
                } else {
                    "Start the daemon."
                }
                .to_owned(),
                next: start.clone(),
            }),
            Self::OldDaemon {
                stop,
                active_workspaces,
                ..
            } => Some(Advice {
                hint: if *active_workspaces == 0 {
                    "This reef is not the running daemon's version; stop the daemon and the next command starts the matching one.".to_owned()
                } else {
                    format!(
                        "This reef is not the running daemon's version, which has {active_workspaces} active workspace{}; stop it when they are done and the next command starts the matching one.",
                        if *active_workspaces == 1 { "" } else { "s" }
                    )
                },
                next: stop.clone(),
            }),
            Self::Refused { advice, .. } | Self::Helper { advice, .. } => advice.clone(),
            Self::NoDaemonBinary { .. } => Some(Advice::hint(
                "Install reef-daemon beside reef: the release tarball, install.sh and Homebrew ship both; from source, `cargo install --git https://github.com/reef-vcs/reef reef-vcs reef-daemon`.",
            )),
            Self::Timeout { .. } => Some(Advice::hint(
                "Retry with a longer --timeout; the request may have finished anyway, so check (reef ws list) before repeating a create.",
            )),
            _ => None,
        }
    }

    /// The process's exit code.
    pub(crate) fn exit_code(&self) -> u8 {
        self.kind().exit_code()
    }

    /// The daemon's `ErrorDetail.reason`, empty when there is none.
    pub(crate) fn reason(&self) -> &str {
        match self {
            Self::Daemon(error) => error.reason(),
            Self::OldDaemon { error, .. } => error.reason(),
            Self::Helper { reason, .. } => reason,
            _ => "",
        }
    }
}

impl Classify for CliError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Daemon(error) => error.kind(),
            Self::OldDaemon { error, .. } => error.kind(),
            Self::NotRunning { .. } | Self::Autostart { .. } | Self::Timeout { .. } => {
                ErrorKind::Unavailable
            }
            Self::Config(error) => error.kind(),
            Self::Logging(error) => error.kind(),
            Self::Runtime(_) | Self::Signals(_) | Self::HelperFailed { .. } => ErrorKind::Internal,
            Self::Refused { kind, .. } | Self::Helper { kind, .. } => *kind,
            Self::NoDaemonBinary { .. } => ErrorKind::NotFound,
            Self::Usage(_) => ErrorKind::InvalidArgument,
            Self::Io { source, .. } | Self::Stdout(source) => classify_by_source(source),
        }
    }
}

/// The JSON error object's `kind`: the kind's name in `snake_case`.
pub(crate) use reef_client::error::kind_text;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_readme_table() {
        let cases = [
            (ErrorKind::InvalidArgument, 2, "invalid_argument"),
            (ErrorKind::NotFound, 3, "not_found"),
            (ErrorKind::AlreadyExists, 4, "already_exists"),
            (ErrorKind::Conflict, 5, "conflict"),
            (ErrorKind::Unavailable, 6, "unavailable"),
            (ErrorKind::PermissionDenied, 7, "permission_denied"),
            (ErrorKind::Io, 8, "io"),
            (ErrorKind::Unsupported, 9, "unsupported"),
            (ErrorKind::Internal, 1, "internal"),
        ];
        for (kind, code, text) in cases {
            let error = CliError::Refused {
                kind,
                message: String::new(),
                advice: None,
            };
            assert_eq!(error.exit_code(), code, "{kind:?}");
            assert_eq!(kind_text(kind), text);
        }
        let not_running = CliError::NotRunning {
            socket: "/s".into(),
            start: Vec::new(),
            autostart_off: true,
        };
        assert_eq!(not_running.exit_code(), 6);
    }
}
