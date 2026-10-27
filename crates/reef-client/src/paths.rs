//! Where the daemon listens, settled exactly as the daemon settles it
//! (ADR 0013): the daemon's `Config::load` resolves its paths through
//! [`Layered`], and a client through [`ClientConfig::load`], so the two
//! never disagree about the socket (R2.05).
//!
//! Precedence, lowest first: the built-in default, `config.toml`'s
//! `[daemon]`, the environment (`REEF_STATE_DIR`, `REEF_SOCKET`,
//! `REEF_CONFIG`), then a [`PathOverrides`] field (the binaries' flags).
//!
//! Platform defaults:
//!
//! | | Linux | macOS |
//! | --- | --- | --- |
//! | state directory | `$XDG_STATE_HOME/reef` (default `~/.local/state/reef`) | `~/Library/Application Support/reef` |
//! | socket | `$XDG_RUNTIME_DIR/reef/daemon.sock`, else `<state>/daemon.sock` | `<state>/daemon.sock` |
//! | config file | `<state>/config.toml` | `<state>/config.toml` |
//!
//! The `$XDG_RUNTIME_DIR` socket is the default only for the default state
//! directory. A daemon given its own state directory keeps its socket in it,
//! so two daemons on two state directories never contend for one socket.
//!
//! A client reads three keys of `config.toml`, `daemon.socket`,
//! `daemon.pid_file` and `repo.auto_register`, and ignores the rest: the
//! daemon owns, and validates, everything else.

use std::ffi::OsString;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use reef_error::{Classify, ErrorKind};
use serde::{Deserialize, Serialize};

/// Environment variable overriding the state directory.
pub const STATE_DIR_ENV: &str = "REEF_STATE_DIR";
/// Environment variable overriding the socket path.
pub const SOCKET_ENV: &str = "REEF_SOCKET";
/// Environment variable overriding the config file path.
pub const CONFIG_ENV: &str = "REEF_CONFIG";

/// The longest socket path accepted, in bytes. `sockaddr_un.sun_path` is 104
/// bytes on macOS (108 on Linux) including the terminating NUL; the stricter
/// limit applies on both platforms.
pub const SOCKET_PATH_MAX: usize = 103;

/// Paths given explicitly, which win over the environment.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathOverrides {
    /// The state directory.
    pub state_dir: Option<PathBuf>,
    /// The socket path.
    pub socket: Option<PathBuf>,
    /// The config file.
    pub config_file: Option<PathBuf>,
}

/// The layer a configuration value came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The built-in default.
    Default,
    /// `config.toml`.
    File,
    /// An environment variable.
    Env,
    /// A command-line flag ([`PathOverrides`]).
    Flag,
}

impl Source {
    /// `default`, `file`, `env`, or `flag`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::File => "file",
            Self::Env => "env",
            Self::Flag => "flag",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Failure to settle the paths or read the client's keys. The daemon's
/// `ConfigError` has a variant of the same name and message for each.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum PathsError {
    /// No home directory to put the default state directory under.
    #[error("cannot find the home directory for the default state directory; set {STATE_DIR_ENV}")]
    NoHome,
    /// The socket path does not fit in `sockaddr_un`.
    #[error(
        "socket path {} is {} bytes; the limit is {max}. Set {SOCKET_ENV} or use a shorter state directory",
        path.display(),
        path.as_os_str().len()
    )]
    SocketPathTooLong {
        /// The path.
        path: PathBuf,
        /// [`SOCKET_PATH_MAX`].
        max: usize,
    },
    /// A relative path could not be made absolute.
    #[error("resolve {}", path.display())]
    Io {
        /// The path as given.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The config file exists but could not be read.
    #[error("read the configuration file {}", path.display())]
    Read {
        /// The file.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// A config file named by `REEF_CONFIG` or `--config` does not exist.
    #[error(
        "the configuration file {} does not exist (named by {})",
        path.display(),
        if *source_layer == Source::Flag { "--config" } else { CONFIG_ENV }
    )]
    Missing {
        /// The file.
        path: PathBuf,
        /// [`Source::Env`] or [`Source::Flag`].
        source_layer: Source,
    },
    /// The config file is not valid TOML, or one of the client's keys has
    /// the wrong type.
    #[error("parse the configuration file {}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// toml's error, with the offending line and a caret under it.
        #[source]
        source: Box<toml::de::Error>,
    },
}

impl Classify for PathsError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NoHome | Self::SocketPathTooLong { .. } | Self::Parse { .. } => {
                ErrorKind::InvalidArgument
            }
            Self::Missing { .. } => ErrorKind::NotFound,
            Self::Io { source, .. } | Self::Read { source, .. } => {
                if source.kind() == io::ErrorKind::PermissionDenied {
                    ErrorKind::PermissionDenied
                } else {
                    ErrorKind::Io
                }
            }
        }
    }
}

/// The platform's default state directory, and on Linux the runtime
/// directory the default socket goes in.
#[derive(Clone, Debug)]
pub struct PlatformDefaults {
    /// `~/Library/Application Support/reef`, or `$XDG_STATE_HOME/reef`.
    pub state_dir: PathBuf,
    /// `$XDG_RUNTIME_DIR` on Linux, when set.
    pub runtime_dir: Option<PathBuf>,
}

/// This platform's defaults, from the home directory and XDG variables.
#[cfg(target_os = "macos")]
pub fn platform_defaults() -> Result<PlatformDefaults, PathsError> {
    use etcetera::base_strategy::{Apple, BaseStrategy as _};
    let apple = Apple::new().map_err(|_| PathsError::NoHome)?;
    Ok(PlatformDefaults {
        state_dir: apple.data_dir().join("reef"),
        runtime_dir: None,
    })
}

/// This platform's defaults, from the home directory and XDG variables.
#[cfg(not(target_os = "macos"))]
pub fn platform_defaults() -> Result<PlatformDefaults, PathsError> {
    use etcetera::base_strategy::{BaseStrategy as _, Xdg};
    let xdg = Xdg::new().map_err(|_| PathsError::NoHome)?;
    let state = xdg
        .state_dir()
        .unwrap_or_else(|| xdg.home_dir().join(".local/state"));
    Ok(PlatformDefaults {
        state_dir: state.join("reef"),
        runtime_dir: xdg.runtime_dir(),
    })
}

/// `path` made absolute against the current directory, without touching the
/// filesystem.
pub fn absolute(path: &Path) -> Result<PathBuf, PathsError> {
    std::path::absolute(path).map_err(|source| PathsError::Io {
        path: path.to_owned(),
        source,
    })
}

/// The paths settled before the config file is read: the file's location
/// depends on them.
#[derive(Clone, Debug)]
pub struct Layered {
    /// The state directory.
    pub state_dir: PathBuf,
    /// Where it came from.
    pub state_dir_source: Source,
    /// The config file.
    pub config_file: PathBuf,
    /// Where it came from.
    pub config_file_source: Source,
    /// From a flag or the environment; the file's `daemon.socket` and the
    /// default come later.
    pub socket: Option<(PathBuf, Source)>,
    /// The platform's runtime directory, for the default state directory
    /// only.
    pub runtime_dir: Option<PathBuf>,
}

/// The paths with the file's `[daemon]` section applied.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settled {
    /// The state directory.
    pub state_dir: PathBuf,
    /// The config file.
    pub config_file: PathBuf,
    /// The socket.
    pub socket: PathBuf,
    /// Where it came from.
    pub socket_source: Source,
    /// The pid file.
    pub pid: PathBuf,
    /// Where it came from.
    pub pid_source: Source,
}

impl Layered {
    /// The state directory, the config file, and a socket given by a flag
    /// or the environment, from `overrides`, `env`, and, for the state
    /// directory when neither gives one, `defaults`.
    pub fn resolve(
        overrides: &PathOverrides,
        env: impl Fn(&str) -> Option<OsString>,
        defaults: impl FnOnce() -> Result<PlatformDefaults, PathsError>,
    ) -> Result<Layered, PathsError> {
        let given = |field: &Option<PathBuf>, var: &str| -> Result<_, PathsError> {
            let (path, source) = if let Some(path) = field {
                (path.clone(), Source::Flag)
            } else if let Some(value) = env(var).filter(|v| !v.is_empty()) {
                (PathBuf::from(value), Source::Env)
            } else {
                return Ok(None);
            };
            Ok(Some((absolute(&path)?, source)))
        };
        let state_dir = given(&overrides.state_dir, STATE_DIR_ENV)?;
        let socket = given(&overrides.socket, SOCKET_ENV)?;
        let config_file = given(&overrides.config_file, CONFIG_ENV)?;

        let (state_dir, state_dir_source, runtime_dir) = if let Some((dir, source)) = state_dir {
            (dir, source, None)
        } else {
            let defaults = defaults()?;
            (defaults.state_dir, Source::Default, defaults.runtime_dir)
        };
        let (config_file, config_file_source) =
            config_file.unwrap_or_else(|| (state_dir.join("config.toml"), Source::Default));
        Ok(Layered {
            state_dir,
            state_dir_source,
            config_file,
            config_file_source,
            socket,
            runtime_dir,
        })
    }

    /// The final paths with the file's `daemon.socket` and
    /// `daemon.pid_file` applied: relative to the state directory, below a
    /// flag or the environment for the socket.
    pub fn finish(
        self,
        file_socket: Option<&Path>,
        file_pid: Option<&Path>,
    ) -> Result<Settled, PathsError> {
        let state_dir = self.state_dir;
        // `join` keeps an absolute path as it is.
        let (socket, socket_source) = match (self.socket, file_socket) {
            (Some(given), _) => given,
            (None, Some(path)) => (state_dir.join(path), Source::File),
            (None, None) => {
                let socket = match self.runtime_dir {
                    Some(runtime) => runtime.join("reef").join("daemon.sock"),
                    None => state_dir.join("daemon.sock"),
                };
                (socket, Source::Default)
            }
        };
        if socket.as_os_str().len() > SOCKET_PATH_MAX {
            return Err(PathsError::SocketPathTooLong {
                path: socket,
                max: SOCKET_PATH_MAX,
            });
        }
        let (pid, pid_source) = match file_pid {
            Some(path) => (state_dir.join(path), Source::File),
            None => (state_dir.join("daemon.pid"), Source::Default),
        };
        Ok(Settled {
            state_dir,
            config_file: self.config_file,
            socket,
            socket_source,
            pid,
            pid_source,
        })
    }
}

/// What a client needs from the configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientConfig {
    /// The state directory.
    pub state_dir: PathBuf,
    /// The configuration file; a missing file means defaults.
    pub config_file: PathBuf,
    /// The daemon's socket.
    pub socket: PathBuf,
    /// The daemon's pid file, present while it runs.
    pub pid: PathBuf,
    /// `[repo] auto_register`: a command given a path in no registered
    /// repository registers it and retries once. Default true.
    pub auto_register: bool,
}

/// The keys a client reads; every other key is ignored, not refused.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ClientFile {
    daemon: ClientDaemon,
    repo: ClientRepo,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct ClientDaemon {
    socket: Option<PathBuf>,
    pid_file: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(default)]
struct ClientRepo {
    auto_register: bool,
}

impl Default for ClientRepo {
    fn default() -> Self {
        Self {
            auto_register: true,
        }
    }
}

impl ClientConfig {
    /// Settles the paths as the daemon does and reads the client's three
    /// keys from the config file. Reads the environment; creates nothing.
    pub fn load(overrides: &PathOverrides) -> Result<ClientConfig, PathsError> {
        Self::load_with(overrides, |name| std::env::var_os(name), platform_defaults)
    }

    /// [`ClientConfig::load`] with the environment and the platform
    /// defaults given, for tests.
    pub fn load_with(
        overrides: &PathOverrides,
        env: impl Fn(&str) -> Option<OsString>,
        defaults: impl FnOnce() -> Result<PlatformDefaults, PathsError>,
    ) -> Result<ClientConfig, PathsError> {
        let layered = Layered::resolve(overrides, env, defaults)?;
        let path = layered.config_file.clone();
        let file = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str::<ClientFile>(&text).map_err(|source| PathsError::Parse {
                    path: path.clone(),
                    source: Box::new(source),
                })?
            }
            // Only the implicit `<state>/config.toml` may be absent: a file
            // named by `REEF_CONFIG` or `--config` that is missing is a typo.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                if layered.config_file_source != Source::Default {
                    return Err(PathsError::Missing {
                        path,
                        source_layer: layered.config_file_source,
                    });
                }
                ClientFile::default()
            }
            Err(source) => return Err(PathsError::Read { path, source }),
        };
        let settled = layered.finish(
            file.daemon.socket.as_deref(),
            file.daemon.pid_file.as_deref(),
        )?;
        Ok(ClientConfig {
            state_dir: settled.state_dir,
            config_file: settled.config_file,
            socket: settled.socket,
            pid: settled.pid,
            auto_register: file.repo.auto_register,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn load(dir: &Path, file: &str) -> Result<ClientConfig, PathsError> {
        std::fs::write(dir.join("config.toml"), file).unwrap();
        let overrides = PathOverrides {
            state_dir: Some(dir.to_owned()),
            ..PathOverrides::default()
        };
        ClientConfig::load_with(&overrides, |_| None, || Err(PathsError::NoHome))
    }

    #[test]
    fn client_config_is_lenient() {
        let tmp = tempfile::tempdir().unwrap();
        let config = load(
            tmp.path(),
            "[watcher]\nnope = 1\n[lease]\ndefault = \"forever\"\n",
        )
        .unwrap();
        assert!(config.auto_register);
        assert_eq!(config.socket, tmp.path().join("daemon.sock"));

        let config = load(
            tmp.path(),
            "[repo]\nauto_register = false\n[daemon]\nsocket = \"s.sock\"\npid_file = \"p\"\n",
        )
        .unwrap();
        assert!(!config.auto_register);
        assert_eq!(config.socket, tmp.path().join("s.sock"));
        assert_eq!(config.pid, tmp.path().join("p"));

        let error = load(tmp.path(), "[repo]\nauto_register = \"yes\"\n").unwrap_err();
        assert!(matches!(error, PathsError::Parse { .. }), "{error:?}");
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        let error = load(tmp.path(), "[repo\n").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    }

    #[test]
    fn a_named_file_must_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let named = tmp.path().join("absent.toml");
        let vars: HashMap<&str, OsString> = [
            (STATE_DIR_ENV, tmp.path().into()),
            (CONFIG_ENV, named.clone().into()),
        ]
        .into();
        let error = ClientConfig::load_with(
            &PathOverrides::default(),
            |name| vars.get(name).cloned(),
            || Err(PathsError::NoHome),
        )
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::NotFound);
        assert_eq!(
            error.to_string(),
            format!(
                "the configuration file {} does not exist (named by REEF_CONFIG)",
                named.display()
            )
        );
        // The implicit file may be absent.
        let config = ClientConfig::load_with(
            &PathOverrides {
                state_dir: Some(tmp.path().to_owned()),
                ..PathOverrides::default()
            },
            |_| None,
            || Err(PathsError::NoHome),
        )
        .unwrap();
        assert!(config.auto_register);
    }
}
