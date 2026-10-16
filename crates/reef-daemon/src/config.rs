//! Where the daemon keeps its state, and the configuration it runs with.
//!
//! R1.03 introduces the paths and logging options only. R1.04 adds the
//! `config.toml` file and its sections without changing [`Paths`].
//!
//! Resolution, highest precedence first: a [`PathOverrides`] field (the
//! binary's flags), the environment (`REEF_STATE_DIR`, `REEF_SOCKET`,
//! `REEF_CONFIG`), then the platform default:
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

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::settings::Identity;

use crate::logging::LogOptions;
use crate::model::RepoId;

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

/// Every path the daemon uses, resolved and absolute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// The state directory.
    pub state_dir: PathBuf,
    /// The Unix socket the API listens on.
    pub socket: PathBuf,
    /// The configuration file (read from R1.04 on; a missing file means
    /// defaults).
    pub config_file: PathBuf,
    /// The metadata database, `<state>/reef.sqlite3`.
    pub db: PathBuf,
    /// The single-instance lock, `<state>/daemon.lock`.
    pub lock: PathBuf,
    /// The pid file, `<state>/daemon.pid`, present while the daemon runs.
    pub pid: PathBuf,
    /// The log directory, `<state>/logs`.
    pub logs: PathBuf,
    /// Per-repository state, `<state>/repos`.
    pub repos: PathBuf,
}

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

/// The daemon's configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where everything lives.
    pub paths: Paths,
    /// Logging, used by [`crate::run_main`]; [`crate::Daemon::start`] does
    /// not install a subscriber.
    pub log: LogOptions,
}

/// Failure to resolve the configuration.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
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
        source: std::io::Error,
    },
}

impl Classify for ConfigError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NoHome | Self::SocketPathTooLong { .. } => ErrorKind::InvalidArgument,
            Self::Io { .. } => ErrorKind::Io,
        }
    }
}

impl Paths {
    /// Resolves every path from `overrides`, the environment, and the
    /// platform defaults (see the module docs). Nothing is created.
    pub fn resolve(overrides: &PathOverrides) -> Result<Paths, ConfigError> {
        Self::resolve_with(overrides, |name| std::env::var_os(name), platform_defaults)
    }

    fn resolve_with(
        overrides: &PathOverrides,
        env: impl Fn(&str) -> Option<OsString>,
        defaults: impl FnOnce() -> Result<PlatformDefaults, ConfigError>,
    ) -> Result<Paths, ConfigError> {
        let given = |field: &Option<PathBuf>, var: &str| {
            field
                .clone()
                .or_else(|| env(var).filter(|v| !v.is_empty()).map(PathBuf::from))
                .map(|path| absolute(&path))
                .transpose()
        };
        let state_dir = given(&overrides.state_dir, STATE_DIR_ENV)?;
        let socket = given(&overrides.socket, SOCKET_ENV)?;
        let config_file = given(&overrides.config_file, CONFIG_ENV)?;

        let (state_dir, runtime_dir) = if let Some(dir) = state_dir {
            (dir, None)
        } else {
            let defaults = defaults()?;
            (defaults.state_dir, defaults.runtime_dir)
        };
        let socket = socket.unwrap_or_else(|| match runtime_dir {
            Some(runtime) => runtime.join("reef").join("daemon.sock"),
            None => state_dir.join("daemon.sock"),
        });
        if socket.as_os_str().len() > SOCKET_PATH_MAX {
            return Err(ConfigError::SocketPathTooLong {
                path: socket,
                max: SOCKET_PATH_MAX,
            });
        }
        Ok(Paths {
            config_file: config_file.unwrap_or_else(|| state_dir.join("config.toml")),
            db: state_dir.join("reef.sqlite3"),
            lock: state_dir.join("daemon.lock"),
            pid: state_dir.join("daemon.pid"),
            logs: state_dir.join("logs"),
            repos: state_dir.join("repos"),
            socket,
            state_dir,
        })
    }

    /// Paths under `state_dir` alone, ignoring the environment: the socket
    /// is `<state>/daemon.sock` and the config file `<state>/config.toml`.
    /// For tests and tools that run a daemon in a directory of their own.
    pub fn in_state_dir(state_dir: &Path) -> Result<Paths, ConfigError> {
        let overrides = PathOverrides {
            state_dir: Some(state_dir.to_owned()),
            ..PathOverrides::default()
        };
        Self::resolve_with(&overrides, |_| None, || Err(ConfigError::NoHome))
    }

    /// The state directory of one repository, `<state>/repos/<repo-id>`.
    pub fn repo_dir(&self, id: RepoId) -> PathBuf {
        self.repos.join(id.to_string())
    }
}

impl Config {
    /// The configuration for `paths` with logging read from the environment
    /// and the log file at `<state>/logs/daemon.log`.
    pub fn new(paths: Paths, mut log: LogOptions) -> Config {
        log.file = Some(paths.logs.join(crate::logging::LOG_FILE_PREFIX));
        Config { paths, log }
    }
}

/// The identity recorded on commits the daemon writes. R1.04 replaces this
/// with the `[identity]` section; the default stays `reef <reef@hostname>`.
pub fn default_identity() -> Identity {
    let host = nix::unistd::gethostname()
        .ok()
        .and_then(|name| name.into_string().ok())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "localhost".to_owned());
    Identity {
        name: "reef".to_owned(),
        email: format!("reef@{host}"),
    }
}

struct PlatformDefaults {
    state_dir: PathBuf,
    runtime_dir: Option<PathBuf>,
}

#[cfg(target_os = "macos")]
fn platform_defaults() -> Result<PlatformDefaults, ConfigError> {
    use etcetera::base_strategy::{Apple, BaseStrategy as _};
    let apple = Apple::new().map_err(|_| ConfigError::NoHome)?;
    Ok(PlatformDefaults {
        state_dir: apple.data_dir().join("reef"),
        runtime_dir: None,
    })
}

#[cfg(not(target_os = "macos"))]
fn platform_defaults() -> Result<PlatformDefaults, ConfigError> {
    use etcetera::base_strategy::{BaseStrategy as _, Xdg};
    let xdg = Xdg::new().map_err(|_| ConfigError::NoHome)?;
    let state = xdg
        .state_dir()
        .unwrap_or_else(|| xdg.home_dir().join(".local/state"));
    Ok(PlatformDefaults {
        state_dir: state.join("reef"),
        runtime_dir: xdg.runtime_dir(),
    })
}

fn absolute(path: &Path) -> Result<PathBuf, ConfigError> {
    std::path::absolute(path).map_err(|source| ConfigError::Io {
        path: path.to_owned(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn resolve(
        overrides: &PathOverrides,
        vars: &[(&str, &str)],
        runtime_dir: Option<&str>,
    ) -> Result<Paths, ConfigError> {
        let vars: HashMap<_, _> = vars.iter().copied().collect();
        Paths::resolve_with(
            overrides,
            |name| vars.get(name).map(OsString::from),
            || {
                Ok(PlatformDefaults {
                    state_dir: PathBuf::from("/home/u/.local/state/reef"),
                    runtime_dir: runtime_dir.map(PathBuf::from),
                })
            },
        )
    }

    #[test]
    fn defaults_and_layout() {
        let paths = resolve(&PathOverrides::default(), &[], None).unwrap();
        let state = Path::new("/home/u/.local/state/reef");
        assert_eq!(paths.state_dir, state);
        assert_eq!(paths.socket, state.join("daemon.sock"));
        assert_eq!(paths.config_file, state.join("config.toml"));
        assert_eq!(paths.db, state.join("reef.sqlite3"));
        assert_eq!(paths.lock, state.join("daemon.lock"));
        assert_eq!(paths.pid, state.join("daemon.pid"));
        assert_eq!(paths.logs, state.join("logs"));
        let id = RepoId::generate();
        assert_eq!(paths.repo_dir(id), state.join("repos").join(id.to_string()));
    }

    #[test]
    fn runtime_dir_only_for_the_default_state_dir() {
        let paths = resolve(&PathOverrides::default(), &[], Some("/run/user/1")).unwrap();
        assert_eq!(paths.socket, Path::new("/run/user/1/reef/daemon.sock"));
        let paths = resolve(
            &PathOverrides::default(),
            &[(STATE_DIR_ENV, "/tmp/s")],
            Some("/run/user/1"),
        )
        .unwrap();
        assert_eq!(paths.socket, Path::new("/tmp/s/daemon.sock"));
    }

    #[test]
    fn overrides_win_over_env() {
        let overrides = PathOverrides {
            state_dir: Some("/a".into()),
            socket: None,
            config_file: Some("/c.toml".into()),
        };
        let paths = resolve(
            &overrides,
            &[
                (STATE_DIR_ENV, "/b"),
                (SOCKET_ENV, "/s.sock"),
                (CONFIG_ENV, "/d.toml"),
            ],
            None,
        )
        .unwrap();
        assert_eq!(paths.state_dir, Path::new("/a"));
        assert_eq!(paths.socket, Path::new("/s.sock"));
        assert_eq!(paths.config_file, Path::new("/c.toml"));
    }

    #[test]
    fn empty_env_is_unset_and_relative_is_absolutized() {
        let paths = resolve(&PathOverrides::default(), &[(STATE_DIR_ENV, "")], None).unwrap();
        assert_eq!(paths.state_dir, Path::new("/home/u/.local/state/reef"));
        let paths = resolve(&PathOverrides::default(), &[(STATE_DIR_ENV, "rel")], None).unwrap();
        assert!(paths.state_dir.is_absolute());
        assert!(paths.state_dir.ends_with("rel"));
    }

    #[test]
    fn socket_length_limit() {
        let ok = format!("/{}", "s".repeat(SOCKET_PATH_MAX - 1));
        assert!(resolve(&PathOverrides::default(), &[(SOCKET_ENV, &ok)], None).is_ok());
        let long = format!("/{}", "s".repeat(SOCKET_PATH_MAX));
        let err = resolve(&PathOverrides::default(), &[(SOCKET_ENV, &long)], None).unwrap_err();
        assert!(
            matches!(err, ConfigError::SocketPathTooLong { max: SOCKET_PATH_MAX, ref path } if path.as_os_str().len() == SOCKET_PATH_MAX + 1)
        );
        assert_eq!(err.kind(), ErrorKind::InvalidArgument);
    }

    #[test]
    fn in_state_dir_ignores_the_environment() {
        let paths = Paths::in_state_dir(Path::new("/tmp/reef-x")).unwrap();
        assert_eq!(paths.socket, Path::new("/tmp/reef-x/daemon.sock"));
    }
}
