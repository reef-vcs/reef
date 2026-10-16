//! Logging initialization shared by the `reef-daemon` and `reef` binaries
//! (ADR 0009). Libraries only emit events; this module installs the one
//! global subscriber.
//!
//! Logs go to stderr. `REEF_LOG` (falling back to `RUST_LOG`, then `info`)
//! is an [`EnvFilter`] directive. `REEF_LOG_FORMAT` is `auto`, `pretty`, or
//! `json`; `auto` means pretty when stderr is a terminal and JSON otherwise.
//!
//! The daemon also writes [`LogOptions::file`], always as JSON lines, through
//! the same filter: `<state>/logs/daemon.log.<YYYY-MM-DD>`, rotated daily
//! (UTC), keeping the [`LogOptions::keep_days`] most recent files. Older
//! files are pruned when the log opens and at each rotation. The file is
//! written by a background thread through `tracing-appender`'s non-blocking
//! writer, which drops lines rather than block when its buffer (128,000
//! lines) is full; that is acceptable for a diagnostic log. Stderr is
//! written synchronously and never drops.

use std::ffi::OsString;
use std::io::IsTerminal as _;
use std::os::unix::fs::DirBuilderExt as _;
use std::path::PathBuf;
use std::str::FromStr;

use reef_lib::error::{Classify, ErrorKind};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};
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
/// The daemon log's file name before the date suffix.
pub const LOG_FILE_PREFIX: &str = "daemon.log";
/// How many daily log files are kept by default (`log.keep_days`).
pub const DEFAULT_KEEP_DAYS: u32 = 7;

/// What to log and how to format it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogOptions {
    /// An [`EnvFilter`] directive such as `info` or `reef_lib=debug`.
    /// `None` means [`DEFAULT_FILTER`].
    pub filter: Option<String>,
    /// The output format on stderr.
    pub format: LogFormat,
    /// A JSON log file, such as `<state>/logs/daemon.log`, rotated daily
    /// into `daemon.log.<date>`. `None` logs to stderr only.
    pub file: Option<PathBuf>,
    /// How many daily files of [`LogOptions::file`] to keep, at least 1.
    pub keep_days: u32,
}

impl Default for LogOptions {
    fn default() -> Self {
        Self {
            filter: None,
            format: LogFormat::default(),
            file: None,
            keep_days: DEFAULT_KEEP_DAYS,
        }
    }
}

/// Keeps the log file's writer thread alive; dropping it flushes the lines
/// still buffered. Hold it until the process exits.
#[must_use = "dropping the guard stops the log file writer"]
#[derive(Debug)]
pub struct LoggingGuard {
    _file: Option<WorkerGuard>,
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
    /// The log directory could not be created.
    #[error("create log directory {}", path.display())]
    LogDir {
        /// The directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The log file could not be opened.
    #[error("open log file {}", path.display())]
    File {
        /// The log file, before its date suffix.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: InitError,
    },
}

impl Classify for LoggingError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NotUnicode { .. } | Self::InvalidFormat(_) | Self::InvalidFilter { .. } => {
                ErrorKind::InvalidArgument
            }
            Self::AlreadySet(_) => ErrorKind::Internal,
            Self::File { .. } | Self::LogDir { .. } => ErrorKind::Io,
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
        Ok(Self {
            filter,
            format,
            ..Self::default()
        })
    }
}

/// Installs the global subscriber. Returns an error if one is already set.
pub fn init(opts: &LogOptions) -> Result<LoggingGuard, LoggingError> {
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
    let (file_layer, file_guard) = match &opts.file {
        Some(path) => {
            let (writer, guard) =
                tracing_appender::non_blocking(open_log_file(path, opts.keep_days)?);
            let layer = fmt::layer()
                .json()
                .flatten_event(true)
                .with_current_span(true)
                .with_span_list(false)
                .with_target(true)
                .with_timer(UtcTime::rfc_3339())
                .with_writer(writer);
            (Some(layer), Some(guard))
        }
        None => (None, None),
    };
    tracing_subscriber::registry()
        .with(filter)
        .with(json_layer)
        .with(pretty_layer)
        .with(file_layer)
        .try_init()
        .map_err(LoggingError::AlreadySet)?;
    tracing::debug!("logging initialized");
    Ok(LoggingGuard { _file: file_guard })
}

/// Opens `path`'s directory as a daily-rotated appender named after `path`'s
/// file name, creating the directory, and prunes all but the `keep_days`
/// most recent files.
fn open_log_file(
    path: &std::path::Path,
    keep_days: u32,
) -> Result<RollingFileAppender, LoggingError> {
    let error = |source| LoggingError::File {
        path: path.to_owned(),
        source,
    };
    let dir = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let prefix = path
        .file_name()
        .map_or_else(|| LOG_FILE_PREFIX.into(), |name| name.to_string_lossy());
    // tracing-appender prunes before it creates the directory, and prints
    // to stderr if the directory is missing. Logs are private like the rest
    // of the state directory.
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|source| LoggingError::LogDir {
            path: dir.to_owned(),
            source,
        })?;
    RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix(prefix)
        .max_log_files(usize::try_from(keep_days.max(1)).unwrap_or(usize::MAX))
        .build(dir)
        .map_err(error)
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

    #[test]
    fn opening_the_log_creates_its_dir_and_keeps_the_newest_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("logs");
        std::fs::create_dir(&dir).unwrap();
        for day in ["2020-01-01", "2020-01-02", "2020-01-03"] {
            std::fs::write(dir.join(format!("{LOG_FILE_PREFIX}.{day}")), "{}\n").unwrap();
        }
        std::fs::write(dir.join("unrelated.txt"), "").unwrap();
        let _appender = open_log_file(&dir.join(LOG_FILE_PREFIX), 2).unwrap();
        let mut names: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        // Today's file is created and counts toward the two kept.
        assert_eq!(names.len(), 3, "{names:?}");
        assert_eq!(names[0], "daemon.log.2020-01-03", "{names:?}");
        assert!(names[1].starts_with("daemon.log.20") && names[1] != names[0]);
        assert_eq!(names[2], "unrelated.txt");

        let fresh = tmp.path().join("new").join("logs");
        let _appender = open_log_file(&fresh.join(LOG_FILE_PREFIX), 0).unwrap();
        assert!(fresh.is_dir());
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
