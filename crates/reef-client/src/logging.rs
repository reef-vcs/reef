//! Logging on stderr for the `reef` and `reef-daemon` binaries (ADR 0009):
//! the variables that configure it, and the stderr layer both install. The
//! daemon's `logging` module adds its log file on top and re-exports these.
//!
//! `REEF_LOG` (falling back to `RUST_LOG`) is an [`EnvFilter`] directive.
//! `REEF_LOG_FORMAT` is `auto`, `pretty`, or `json`; `auto` means pretty
//! when stderr is a terminal and JSON otherwise. Stderr is written
//! synchronously and never drops.

use std::ffi::OsString;
use std::io::IsTerminal as _;
use std::path::PathBuf;
use std::str::FromStr;

use reef_error::{Classify, ErrorKind};
use tracing::Subscriber;
use tracing_subscriber::filter::ParseError;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::{SubscriberInitExt as _, TryInitError};
use tracing_subscriber::{EnvFilter, Layer, fmt};

/// Environment variable holding the filter directive.
pub const LOG_ENV: &str = "REEF_LOG";
/// Environment variable read for the filter when [`LOG_ENV`] is unset,
/// because every Rust user tries it first.
pub const FALLBACK_LOG_ENV: &str = "RUST_LOG";
/// Environment variable selecting the [`LogFormat`].
pub const LOG_FORMAT_ENV: &str = "REEF_LOG_FORMAT";

/// How log lines are written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// [`LogFormat::Pretty`] when stderr is a terminal, [`LogFormat::Json`]
    /// otherwise.
    #[default]
    Auto,
    /// One human-readable line per event.
    Pretty,
    /// One JSON object per event, the format log files and agents read.
    Json,
}

impl FromStr for LogFormat {
    type Err = LoggingError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "pretty" => Ok(Self::Pretty),
            "json" => Ok(Self::Json),
            _ => Err(LoggingError::InvalidFormat(value.to_owned())),
        }
    }
}

/// Failure to read logging options or install the subscriber.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoggingError {
    /// An environment variable was set to a value that is not UTF-8.
    #[error("{var} is not valid UTF-8")]
    NotUnicode {
        /// The variable's name.
        var: &'static str,
    },
    /// The format was not one of `auto`, `pretty`, or `json`.
    #[error("invalid log format {0:?}; expected auto, pretty, or json")]
    InvalidFormat(String),
    /// The filter directive did not parse.
    #[error("invalid log filter {directive:?}")]
    InvalidFilter {
        /// The directive as given.
        directive: String,
        /// Why it did not parse.
        #[source]
        source: ParseError,
    },
    /// A global subscriber was already installed.
    #[error("failed to install the log subscriber")]
    AlreadySet(#[source] TryInitError),
    /// The daemon's log directory is another user's (RX.01, S-01).
    #[error("log directory {} is owned by uid {uid}, not by uid {euid}", path.display())]
    LogDirNotOwned {
        /// The directory.
        path: PathBuf,
        /// Its owner.
        uid: u32,
        /// The daemon's effective uid.
        euid: u32,
    },
    /// The daemon's log directory could not be created.
    #[error("create log directory {}", path.display())]
    LogDir {
        /// The directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The daemon's log file could not be opened.
    #[error("open log file {}", path.display())]
    File {
        /// The log file, before its date suffix.
        path: PathBuf,
        /// The underlying error: `tracing-appender`'s, which this crate
        /// does not depend on.
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl Classify for LoggingError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotUnicode { .. } | Self::InvalidFormat(_) | Self::InvalidFilter { .. } => {
                ErrorKind::InvalidArgument
            }
            Self::AlreadySet(_) => ErrorKind::Internal,
            Self::LogDirNotOwned { .. } => ErrorKind::PermissionDenied,
            Self::File { .. } | Self::LogDir { .. } => ErrorKind::Io,
        }
    }
}

/// What the environment asks for.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EnvLog {
    /// [`LOG_ENV`], else [`FALLBACK_LOG_ENV`]; `None` when neither is set.
    pub filter: Option<String>,
    /// [`LOG_FORMAT_ENV`], [`LogFormat::Auto`] when unset.
    pub format: LogFormat,
}

impl EnvLog {
    /// Reads [`LOG_ENV`], [`FALLBACK_LOG_ENV`], and [`LOG_FORMAT_ENV`]. An
    /// empty variable counts as unset.
    pub fn from_env() -> Result<Self, LoggingError> {
        Self::from_vars(|name| std::env::var_os(name))
    }

    /// [`EnvLog::from_env`] over `var` instead of the environment.
    pub fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Result<Self, LoggingError> {
        let get = |name: &'static str| match var(name) {
            Some(value) if !value.is_empty() => value
                .into_string()
                .map(Some)
                .map_err(|_| LoggingError::NotUnicode { var: name }),
            _ => Ok(None),
        };
        let filter = match get(LOG_ENV)? {
            Some(filter) => Some(filter),
            None => get(FALLBACK_LOG_ENV)?,
        };
        let format = match get(LOG_FORMAT_ENV)? {
            Some(format) => format.parse()?,
            None => LogFormat::Auto,
        };
        Ok(Self { filter, format })
    }
}

/// `directive` as a filter.
pub fn filter(directive: &str) -> Result<EnvFilter, LoggingError> {
    EnvFilter::builder()
        .parse(directive)
        .map_err(|source| LoggingError::InvalidFilter {
            directive: directive.to_owned(),
            source,
        })
}

/// The stderr layer for `format`: JSON lines, or pretty lines coloured only
/// on a terminal and never when `NO_COLOR` is set.
pub fn stderr_layer<S>(format: LogFormat) -> Box<dyn Layer<S> + Send + Sync>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let terminal = std::io::stderr().is_terminal();
    let json = match format {
        LogFormat::Auto => !terminal,
        LogFormat::Pretty => false,
        LogFormat::Json => true,
    };
    if json {
        fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_target(true)
            .with_timer(UtcTime::rfc_3339())
            .with_writer(std::io::stderr)
            .boxed()
    } else {
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        fmt::layer()
            .with_target(false)
            .with_timer(UtcTime::rfc_3339())
            .with_ansi(terminal && !no_color)
            .with_writer(std::io::stderr)
            .boxed()
    }
}

/// Installs the global subscriber, on stderr only. Returns an error if one
/// is already set.
pub fn init(directive: &str, format: LogFormat) -> Result<(), LoggingError> {
    tracing_subscriber::registry()
        .with(filter(directive)?)
        .with(stderr_layer(format))
        .try_init()
        .map_err(LoggingError::AlreadySet)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn options(vars: &[(&str, &str)]) -> Result<EnvLog, LoggingError> {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        EnvLog::from_vars(|name| vars.get(name).map(OsString::from))
    }

    #[test]
    fn defaults_when_unset() {
        assert_eq!(options(&[]).unwrap(), EnvLog::default());
        assert_eq!(options(&[(LOG_ENV, "")]).unwrap(), EnvLog::default());
    }

    #[test]
    fn reef_log_wins_over_rust_log() {
        let opts = options(&[(LOG_ENV, "debug"), (FALLBACK_LOG_ENV, "trace")]).unwrap();
        assert_eq!(opts.filter.as_deref(), Some("debug"));
        let opts = options(&[(FALLBACK_LOG_ENV, "trace")]).unwrap();
        assert_eq!(opts.filter.as_deref(), Some("trace"));
    }

    #[test]
    fn formats_parse() {
        for (text, format) in [
            ("auto", LogFormat::Auto),
            ("pretty", LogFormat::Pretty),
            (" JSON ", LogFormat::Json),
        ] {
            assert_eq!(options(&[(LOG_FORMAT_ENV, text)]).unwrap().format, format);
        }
        let err = options(&[(LOG_FORMAT_ENV, "yaml")]).unwrap_err();
        assert!(matches!(err, LoggingError::InvalidFormat(ref v) if v == "yaml"));
        assert_eq!(err.kind(), ErrorKind::InvalidArgument);
    }

    #[test]
    fn non_unicode_is_an_error() {
        use std::os::unix::ffi::OsStringExt as _;
        let err =
            EnvLog::from_vars(|name| (name == LOG_ENV).then(|| OsString::from_vec(vec![0xff])))
                .unwrap_err();
        assert!(matches!(err, LoggingError::NotUnicode { var: LOG_ENV }));
    }
}
