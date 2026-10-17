//! How a command fails. Every variant has an [`ErrorKind`], which is the
//! exit code (README, CLI: `InvalidArgument` 2, `NotFound` 3,
//! `AlreadyExists` 4, `Conflict` 5, `Unavailable` 6, `PermissionDenied` 7,
//! `Io` 8, `Internal` 1) and the `kind` of the JSON error object.

use std::io;
use std::path::PathBuf;
use std::time::Duration;

use reef_daemon::client::ClientError;
use reef_daemon::config::ConfigError;
use reef_daemon::logging::LoggingError;
use reef_lib::error::{Classify, ErrorKind, classify_by_source};

/// A failed command.
#[derive(Debug, thiserror::Error)]
pub(crate) enum CliError {
    /// The daemon answered with an error, or the connection failed. The
    /// daemon's message is already its whole chain.
    #[error(transparent)]
    Daemon(ClientError),
    /// Nothing answers on the socket and autostart is off.
    #[error("daemon is not running at {}", socket.display())]
    NotRunning {
        /// The socket.
        socket: PathBuf,
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
    Config(#[source] ConfigError),
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
    /// a registered repository (`AlreadyExists`) or a stub (`Internal`).
    #[error("{message}")]
    Refused {
        /// The kind, and so the exit code.
        kind: ErrorKind,
        /// The message.
        message: String,
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
    /// A stub's failure: the command arrives with `ticket`.
    pub(crate) fn not_implemented(ticket: &str) -> Self {
        Self::Refused {
            kind: ErrorKind::Internal,
            message: format!("not implemented until {ticket}"),
        }
    }

    /// The process's exit code.
    pub(crate) fn exit_code(&self) -> u8 {
        self.kind().exit_code()
    }
}

impl Classify for CliError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Daemon(error) => error.kind(),
            Self::NotRunning { .. } | Self::Autostart { .. } | Self::Timeout { .. } => {
                ErrorKind::Unavailable
            }
            Self::Config(error) => error.kind(),
            Self::Logging(error) => error.kind(),
            Self::Runtime(_) | Self::Signals(_) => ErrorKind::Internal,
            Self::Refused { kind, .. } => *kind,
            Self::Usage(_) => ErrorKind::InvalidArgument,
            Self::Io { source, .. } | Self::Stdout(source) => classify_by_source(source),
        }
    }
}

/// The JSON error object's `kind`: the kind's name in `snake_case`.
pub(crate) fn kind_text(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::NotFound => "not_found",
        ErrorKind::AlreadyExists => "already_exists",
        ErrorKind::InvalidArgument => "invalid_argument",
        ErrorKind::Conflict => "conflict",
        ErrorKind::Unavailable => "unavailable",
        ErrorKind::PermissionDenied => "permission_denied",
        ErrorKind::Io => "io",
        _ => "internal",
    }
}

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
            (ErrorKind::Internal, 1, "internal"),
        ];
        for (kind, code, text) in cases {
            let error = CliError::Refused {
                kind,
                message: String::new(),
            };
            assert_eq!(error.exit_code(), code, "{kind:?}");
            assert_eq!(kind_text(kind), text);
        }
        let not_running = CliError::NotRunning {
            socket: "/s".into(),
        };
        assert_eq!(not_running.exit_code(), 6);
        assert_eq!(CliError::not_implemented("R1.17").exit_code(), 1);
    }
}
