//! The executor's errors, with their kind (and so the CLI's exit code) and
//! the fix.

use reef_client::{ClientError, IssueError};
use reef_error::{Advice, Classify as _, ErrorKind};

/// Why the executor stopped, or could not start.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// `executor.toml` or a flag is invalid.
    #[error("{message}")]
    Config {
        /// What is wrong, naming the file and line.
        message: String,
    },
    /// The configured sandbox cannot be applied on this machine.
    #[error("{message}")]
    SandboxUnavailable {
        /// What failed.
        message: String,
        /// How to fix it.
        hint: String,
    },
    /// Another executor uses the work directory.
    #[error("another executor uses {dir}; give this one its own [work] dir")]
    WorkLocked {
        /// The directory.
        dir: String,
    },
    /// A file or directory of the executor's own could not be used.
    #[error("{context}: {source}")]
    Io {
        /// What the executor was doing.
        context: String,
        /// The failure.
        #[source]
        source: std::io::Error,
    },
    /// The daemon refused a call, or could not be reached.
    #[error(transparent)]
    Daemon(Box<ClientError>),
    /// A session key could not be issued: a configuration problem every
    /// later job would hit.
    #[error("no session key for the job: {0}")]
    Key(#[source] Box<IssueError>),
    /// The daemon refused evidence this executor made: a bug in the
    /// executor, which every later job would repeat.
    #[error("the daemon refused evidence {evidence}: {error}")]
    EvidenceRefused {
        /// Which object, by its kind and target.
        evidence: String,
        /// The daemon's error.
        #[source]
        error: Box<ClientError>,
    },
    /// `--once --require-job` and no job came.
    #[error("no job matched within {0}")]
    NoJob(String),
}

impl Error {
    /// The kind, and so the CLI's exit code.
    pub fn kind(&self) -> ErrorKind {
        match self {
            Error::Config { .. } => ErrorKind::InvalidArgument,
            Error::SandboxUnavailable { .. } | Error::WorkLocked { .. } => ErrorKind::Unavailable,
            Error::Io { source, .. } => reef_error::classify_by_source(source),
            Error::Daemon(error) | Error::EvidenceRefused { error, .. } => error.kind(),
            Error::Key(issue) => match &**issue {
                IssueError::Client(error) => error.kind(),
                _ => ErrorKind::Internal,
            },
            Error::NoJob(_) => ErrorKind::NotFound,
        }
    }

    /// How to fix it, when the executor knows.
    pub fn advice(&self) -> Option<Advice> {
        match self {
            Error::SandboxUnavailable { hint, .. } => {
                Some(Advice::run(hint.clone(), ["reef", "executor", "check"]))
            }
            Error::Config { .. } => Some(Advice::run(
                "Fix the file (docs/executor.md lists every key), then check it.",
                ["reef", "executor", "check"],
            )),
            Error::Daemon(error) => error.advice(),
            Error::Key(issue) => match &**issue {
                IssueError::Client(error) => error.advice(),
                _ => None,
            },
            Error::NoJob(_) => Some(Advice::run(
                "Submit one, or wait longer with --wait.",
                ["reef", "job", "list", "--state", "queued"],
            )),
            _ => None,
        }
    }
}

impl From<ClientError> for Error {
    fn from(error: ClientError) -> Self {
        Error::Daemon(Box::new(error))
    }
}
