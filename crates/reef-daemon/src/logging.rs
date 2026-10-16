//! Logging initialization shared by the `reef-daemon` and `reef` binaries
//! (ADR 0009). Libraries only emit events; this module installs the one
//! global subscriber.
//!
//! Logs go to stderr. `REEF_LOG` (falling back to `RUST_LOG`, then `info`)
//! is an [`EnvFilter`] directive. `REEF_LOG_FORMAT` is `auto`, `pretty`, or
//! `json`; `auto` means pretty when stderr is a terminal and JSON otherwise.

use std::ffi::OsString;
use std::io::IsTerminal as _;
use std::str::FromStr;

use reef_lib::error::{Classify, ErrorKind};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::ParseError;
use tracing_subscriber::fmt;
use tracing_subscriber::fmt::time::UtcTime;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::{SubscriberInitExt as _, TryInitError};

/// Environment variable holding the filter directive.
pub const LOG_ENV: &str = "REEF_LOG";
/// Environment variable read for the filter when [`LOG_ENV`] is unset,
/// because every Rust user tries it first.
pub const FALLBACK_LOG_ENV: &str = "RUST_LOG";
/// Environment variable selecting the [`LogFormat`].
pub const LOG_FORMAT_ENV: &str = "REEF_LOG_FORMAT";
/// The filter used when neither variable is set.
pub const DEFAULT_FILTER: &str = "info";

/// What to log and how to format it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LogOptions {
    /// An [`EnvFilter`] directive such as `info` or `reef_lib=debug`.
    /// `None` means [`DEFAULT_FILTER`].
    pub filter: Option<String>,
    /// The output format.
    pub format: LogFormat,
}

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
}

impl Classify for LoggingError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotUnicode { .. } | Self::InvalidFormat(_) | Self::InvalidFilter { .. } => {
                ErrorKind::InvalidArgument
            }
            Self::AlreadySet(_) => ErrorKind::Internal,
        }
    }
}

impl LogOptions {
    /// Reads [`LOG_ENV`], [`FALLBACK_LOG_ENV`], and [`LOG_FORMAT_ENV`]. An
    /// empty variable counts as unset.
    pub fn from_env() -> Result<Self, LoggingError> {
        Self::from_vars(|name| std::env::var_os(name))
    }

    fn from_vars(var: impl Fn(&str) -> Option<OsString>) -> Result<Self, LoggingError> {
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

/// Installs the global subscriber. Returns an error if one is already set.
pub fn init(opts: &LogOptions) -> Result<(), LoggingError> {
    let directive = opts.filter.as_deref().unwrap_or(DEFAULT_FILTER);
    let filter =
        EnvFilter::builder()
            .parse(directive)
            .map_err(|source| LoggingError::InvalidFilter {
                directive: directive.to_owned(),
                source,
            })?;
    let terminal = std::io::stderr().is_terminal();
    let json = match opts.format {
        LogFormat::Auto => !terminal,
        LogFormat::Pretty => false,
        LogFormat::Json => true,
    };
    let (json_layer, pretty_layer) = if json {
        let layer = fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_target(true)
            .with_timer(UtcTime::rfc_3339())
            .with_writer(std::io::stderr);
        (Some(layer), None)
    } else {
        // Colour only on a terminal, and never when NO_COLOR is set.
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
        let layer = fmt::layer()
            .with_target(false)
            .with_timer(UtcTime::rfc_3339())
            .with_ansi(terminal && !no_color)
            .with_writer(std::io::stderr);
        (None, Some(layer))
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(json_layer)
        .with(pretty_layer)
        .try_init()
        .map_err(LoggingError::AlreadySet)?;
    tracing::debug!("logging initialized");
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn options(vars: &[(&str, &str)]) -> Result<LogOptions, LoggingError> {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        LogOptions::from_vars(|name| vars.get(name).map(OsString::from))
    }

    #[test]
    fn defaults_when_unset() {
        assert_eq!(options(&[]).unwrap(), LogOptions::default());
        assert_eq!(options(&[(LOG_ENV, "")]).unwrap(), LogOptions::default());
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

    #[cfg(unix)]
    #[test]
    fn non_unicode_is_an_error() {
        use std::os::unix::ffi::OsStringExt as _;
        let err =
            LogOptions::from_vars(|name| (name == LOG_ENV).then(|| OsString::from_vec(vec![0xff])))
                .unwrap_err();
        assert!(matches!(err, LoggingError::NotUnicode { var: LOG_ENV }));
    }
}
