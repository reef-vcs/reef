//! Where the daemon keeps its state, and the configuration it runs with
//! (ADR 0013).
//!
//! [`Config::load`] is the one entry point: the daemon and `reef config show`
//! both resolve their configuration through it. It reads nothing of jj's own
//! user configuration.
//!
//! Precedence, lowest first: the built-in default, `config.toml`, the
//! environment (`REEF_STATE_DIR`, `REEF_SOCKET`, `REEF_CONFIG`; the R0.09
//! `REEF_LOG*` variables are read by the binaries), then a [`PathOverrides`]
//! field (the binaries' flags). There is no generic `REEF_<SECTION>_<KEY>`
//! mapping. [`Config::sources`] records which layer won for each key.
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
//! Everything else lives under the state directory: `daemon.lock`,
//! `daemon.pid`, `reef.sqlite3` (with `-wal` and `-shm`), `logs/`,
//! `repos/<repo-id>/bases/<tree-id>/` (with `<tree-id>.state/` beside each),
//! `repos/<repo-id>/ws/<ws-id>/`, `repos/<repo-id>/wc/<ws-id>/`, and
//! `stores/<name>/<lockfile-hash>/`.
//!
//! In `config.toml`, unknown keys are errors, durations are humantime's form
//! (`"500ms"`, `"30m"`, `"7d"`, `"1h 30m"`; printed back in the largest exact
//! unit), sizes are an integer number
//! of bytes or a string with a `B`, `KiB`, `MiB`, or `GiB` unit, and relative
//! paths are relative to the state directory.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt::{self, Write as _};
use std::io;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::time::Duration;

use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::UserSettings;
use reef_lib::jj::settings::{
    Fsmonitor, Identity, SettingsError, SettingsSpec, parse_byte_size, user_settings_from_spec,
};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub use reef_client::paths::{
    CONFIG_ENV, PathOverrides, SOCKET_ENV, SOCKET_PATH_MAX, STATE_DIR_ENV, Source,
};
use reef_client::paths::{Layered, PathsError, PlatformDefaults, platform_defaults};
pub(crate) use reef_client::{machine_hostname, os_username};

use crate::backend::{BackendChoice, OverlayConfig};
use crate::logging::{DEFAULT_KEEP_DAYS, LOG_FILE_PREFIX, LogOptions};
use crate::model::RepoId;

/// Placeholder for the repository id in `workspace.root`.
pub const REPO_ID_PLACEHOLDER: &str = "<repo-id>";
/// Placeholder for the state directory, allowed only at the start of
/// `workspace.root`.
pub const STATE_PLACEHOLDER: &str = "<state>";

/// Every key `config.toml` accepts, by section, in the order
/// [`Config::to_effective_toml`] prints them. A test keeps this in step with
/// [`FileConfig`].
pub const KEYS: &[(&str, &[&str])] = &[
    ("daemon", &["socket", "pid_file"]),
    ("identity", &["name", "email"]),
    ("log", &["keep_days"]),
    (
        "lease",
        &[
            "default",
            "max",
            "grace",
            "restart_grace",
            "heartbeat",
            "tick",
        ],
    ),
    (
        "watcher",
        &[
            "backend",
            "debounce",
            "min_interval",
            "fallback_interval",
            "watched_fallback_interval",
            "narrowed_max_paths",
            "max_concurrent_scans",
            "watchman_threshold_files",
            "watchman_attach_quiet",
            "watchman_attach_defer_max",
            "notify_attach_background",
        ],
    ),
    (
        "workspace",
        &["backend", "root", "fuse_overlayfs", "overlay_nosuid"],
    ),
    ("base_cache", &["max_entries"]),
    ("gc", &["undo_window", "git_executable", "reindex_limit"]),
    (
        "backup",
        &["retention", "quiesce_max", "sqlite_pages_per_step"],
    ),
    ("snapshot", &["max_new_file_size"]),
    ("repo", &["auto_register", "allow_foreign_owner"]),
    ("store", &["allow_builds"]),
];

/// The metadata database's file name in the state directory.
pub const DB_FILE: &str = "reef.sqlite3";
/// The single-instance lock's file name in the state directory.
pub const LOCK_FILE: &str = "daemon.lock";

/// Every path the daemon uses, resolved and absolute.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// The state directory.
    pub state_dir: PathBuf,
    /// The Unix socket the API listens on.
    pub socket: PathBuf,
    /// The configuration file; a missing file means defaults.
    pub config_file: PathBuf,
    /// The metadata database, `<state>/reef.sqlite3`.
    pub db: PathBuf,
    /// The single-instance lock, `<state>/daemon.lock`.
    pub lock: PathBuf,
    /// The pid file, `<state>/daemon.pid` unless `daemon.pid_file` says
    /// otherwise, present while the daemon runs.
    pub pid: PathBuf,
    /// The log directory, `<state>/logs`.
    pub logs: PathBuf,
    /// Per-repository state, `<state>/repos`.
    pub repos: PathBuf,
}

/// `config.toml` as written, every absent key at its default. The `Option`
/// fields default to a value computed at load; [`Config::effective_file`]
/// fills them in.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct FileConfig {
    /// `[daemon]`.
    pub daemon: DaemonSection,
    /// `[identity]`.
    pub identity: IdentitySection,
    /// `[log]`.
    pub log: LogSection,
    /// `[lease]`.
    pub lease: LeaseSection,
    /// `[watcher]`.
    pub watcher: WatcherSection,
    /// `[workspace]`.
    pub workspace: WorkspaceSection,
    /// `[base_cache]`.
    pub base_cache: BaseCacheSection,
    /// `[gc]`.
    pub gc: GcSection,
    /// `[backup]`.
    pub backup: BackupSection,
    /// `[snapshot]`.
    pub snapshot: SnapshotSection,
    /// `[repo]`.
    pub repo: RepoSection,
    /// `[store]`.
    pub store: StoreSection,
}

/// `[daemon]`: where the daemon listens and records its pid.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct DaemonSection {
    /// The socket, below `REEF_SOCKET` and `--socket`. Default: see the
    /// module docs.
    pub socket: Option<PathBuf>,
    /// The pid file. Default `<state>/daemon.pid`.
    pub pid_file: Option<PathBuf>,
}

/// `[identity]`: the author and committer of commits the daemon writes.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct IdentitySection {
    /// Default `reef`.
    pub name: String,
    /// Default `reef@<hostname>`, the full hostname in lowercase.
    pub email: Option<String>,
}

impl Default for IdentitySection {
    fn default() -> Self {
        Self {
            name: "reef".to_owned(),
            email: None,
        }
    }
}

/// `[log]`: the daemon's log file.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct LogSection {
    /// How many daily files of `<state>/logs/daemon.log` to keep, at least 1.
    /// Default 7.
    pub keep_days: u32,
}

impl Default for LogSection {
    fn default() -> Self {
        Self {
            keep_days: DEFAULT_KEEP_DAYS,
        }
    }
}

/// `[lease]`: workspace leases (R1.15).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct LeaseSection {
    /// Lease length when neither the request nor `reef.toml` gives one.
    /// Default 30 minutes; at most [`LeaseSection::max`].
    #[serde(with = "duration")]
    pub default: Duration,
    /// The longest lease a request may ask for. Default 24 hours.
    #[serde(with = "duration")]
    pub max: Duration,
    /// How long after expiry a heartbeat still revives a lease. Default 5
    /// minutes.
    #[serde(with = "duration")]
    pub grace: Duration,
    /// The least time every active lease has left after a daemon start.
    /// Default 5 minutes.
    #[serde(with = "duration")]
    pub restart_grace: Duration,
    /// How often clients should heartbeat. Default 60 seconds.
    #[serde(with = "duration")]
    pub heartbeat: Duration,
    /// How often the daemon looks for expired leases; shorter than
    /// [`LeaseSection::grace`]. Default 10 seconds.
    #[serde(with = "duration")]
    pub tick: Duration,
}

impl Default for LeaseSection {
    fn default() -> Self {
        Self {
            default: Duration::from_mins(30),
            max: Duration::from_hours(24),
            grace: Duration::from_mins(5),
            restart_grace: Duration::from_mins(5),
            heartbeat: Duration::from_secs(60),
            tick: Duration::from_secs(10),
        }
    }
}

/// What triggers snapshots (R1.13, R1.14).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum WatcherBackend {
    /// Watchman for repositories above
    /// [`WatcherSection::watchman_threshold_files`] when `watchman` is on
    /// `PATH`, notify otherwise.
    #[default]
    Auto,
    /// The `notify` crate's watcher.
    Notify,
    /// A Watchman subscription.
    Watchman,
}

impl WatcherBackend {
    /// `auto`, `notify`, or `watchman`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Notify => "notify",
            Self::Watchman => "watchman",
        }
    }
}

/// `[watcher]`: filesystem watching and snapshot pacing (R1.13).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct WatcherSection {
    /// Default `auto`.
    pub backend: WatcherBackend,
    /// Quiet time after the last event before a snapshot. Default 500 ms;
    /// shorter than [`WatcherSection::fallback_interval`].
    #[serde(with = "duration")]
    pub debounce: Duration,
    /// The least time between two snapshots of one workspace. Default 1 s.
    #[serde(with = "duration")]
    pub min_interval: Duration,
    /// A workspace with no event for this long is snapshotted anyway, when
    /// it has no working watch (its watch failed, or a Watchman
    /// subscription is not up) or may hold writes no event reported (its
    /// last automatic snapshot failed, or none has run since a daemon
    /// restart, an aborted release, or a move). Default 60 s.
    #[serde(with = "duration")]
    pub fallback_interval: Duration,
    /// The same for a workspace whose watch is working, which reports every
    /// change (an overflow marks every workspace changed): a full scan, the
    /// backstop for a change a narrowed snapshot missed
    /// ([`WatcherSection::narrowed_max_paths`], R1.30) and for events
    /// dropped while the scheduler was behind. The scheduler
    /// raises it to [`WatcherSection::fallback_interval`] when it is
    /// shorter, so a long `fallback_interval` alone turns both timers down.
    /// Default 10 minutes; at 60 s, the scans of 200 idle 100,000-file
    /// workspaces outran the scan pool (R1.23).
    #[serde(with = "duration")]
    pub watched_fallback_interval: Duration,
    /// A watcher snapshot of a notify workspace scans only the paths
    /// reported since the last snapshot, and everything under them, until
    /// more than this many distinct paths were reported (R1.30, ADR 0026).
    /// Default 1,000; `0` scans every tracked file every time.
    pub narrowed_max_paths: usize,
    /// Scans running at once. Default: the number of CPUs
    /// ([`Config::max_concurrent_scans`]).
    pub max_concurrent_scans: Option<usize>,
    /// Tracked files above which `auto` picks Watchman. Default 50,000.
    pub watchman_threshold_files: u64,
    /// A Watchman workspace's subscription starts only once no workspace
    /// create has started for this long, so Watchman's crawl of it does not
    /// slow the next create's clone (R1.29). Default 2 s; `0s` subscribes
    /// at once.
    #[serde(with = "duration")]
    pub watchman_attach_quiet: Duration,
    /// The longest a new Watchman workspace's subscription waits for that
    /// quiet window, from when it was tracked. Default 10 s; `0s` subscribes
    /// at once.
    #[serde(with = "duration")]
    pub watchman_attach_defer_max: Duration,
    /// On Linux, a created notify workspace's watch attaches after the
    /// create returns, on the watcher's attach thread, and its first
    /// snapshot once the watch is up is a full scan (R1.32, ADR 0028):
    /// inotify adds one watch per directory, about a second for 8,000
    /// directories on a CI runner. `false` attaches inside the create, as
    /// before. Read only on Linux; macOS always attaches inline. Default
    /// true.
    pub notify_attach_background: bool,
}

impl Default for WatcherSection {
    fn default() -> Self {
        Self {
            backend: WatcherBackend::Auto,
            debounce: Duration::from_millis(500),
            min_interval: Duration::from_secs(1),
            fallback_interval: Duration::from_secs(60),
            watched_fallback_interval: Duration::from_mins(10),
            narrowed_max_paths: 1000,
            max_concurrent_scans: None,
            watchman_threshold_files: 50_000,
            watchman_attach_quiet: Duration::from_secs(2),
            watchman_attach_defer_max: Duration::from_secs(10),
            notify_attach_background: true,
        }
    }
}

/// `[workspace]`: where and how workspaces are materialized.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct WorkspaceSection {
    /// Default `auto`.
    pub backend: BackendChoice,
    /// The directory holding a repository's workspaces, a template with
    /// [`REPO_ID_PLACEHOLDER`] and optionally a leading
    /// [`STATE_PLACEHOLDER`]; see [`Config::workspace_root`]. Default
    /// `<state>/repos/<repo-id>/ws`.
    pub root: String,
    /// The `fuse-overlayfs` binary, a path or a name on `PATH`. Default
    /// `fuse-overlayfs`.
    pub fuse_overlayfs: String,
    /// Mount overlays with `nosuid`. Default false.
    pub overlay_nosuid: bool,
}

impl WorkspaceSection {
    /// The overlay backends' settings (R1.09).
    pub fn overlay(&self) -> OverlayConfig {
        OverlayConfig {
            fuse_overlayfs: self.fuse_overlayfs.clone(),
            nosuid: self.overlay_nosuid,
        }
    }
}

impl Default for WorkspaceSection {
    fn default() -> Self {
        Self {
            backend: BackendChoice::Auto,
            root: format!("{STATE_PLACEHOLDER}/repos/{REPO_ID_PLACEHOLDER}/ws"),
            fuse_overlayfs: "fuse-overlayfs".to_owned(),
            overlay_nosuid: false,
        }
    }
}

/// `[base_cache]`: materialized base trees (R1.02, R1.17).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct BaseCacheSection {
    /// Entries GC keeps per repository, at least 1. Default 8.
    pub max_entries: usize,
}

impl Default for BaseCacheSection {
    fn default() -> Self {
        Self { max_entries: 8 }
    }
}

/// `[gc]`: garbage collection (R1.17).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct GcSection {
    /// Operations and released workspaces younger than this are kept.
    /// Default 7 days.
    #[serde(with = "duration")]
    pub undo_window: Duration,
    /// The `git` binary GC runs. Default `git`.
    pub git_executable: String,
    /// Default 100,000.
    pub reindex_limit: u64,
}

impl Default for GcSection {
    fn default() -> Self {
        Self {
            undo_window: Duration::from_hours(7 * 24),
            git_executable: "git".to_owned(),
            reindex_limit: 100_000,
        }
    }
}

/// `[backup]`: local backups (R1.18).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct BackupSection {
    /// How long after a backup GC keeps what it references. Default 30
    /// days.
    #[serde(with = "duration")]
    pub retention: Duration,
    /// A quiesce resumes by itself after this long. Default 60 s.
    #[serde(with = "duration")]
    pub quiesce_max: Duration,
    /// SQLite pages copied per backup step, at least 1. Default 256.
    pub sqlite_pages_per_step: u32,
}

impl Default for BackupSection {
    fn default() -> Self {
        Self {
            retention: Duration::from_hours(30 * 24),
            quiesce_max: Duration::from_secs(60),
            sqlite_pages_per_step: 256,
        }
    }
}

/// `[snapshot]`: what snapshots track.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct SnapshotSection {
    /// The largest new file a snapshot starts tracking; zero means no
    /// limit. Default 1 MiB.
    pub max_new_file_size: ByteSize,
}

impl Default for SnapshotSection {
    fn default() -> Self {
        Self {
            max_new_file_size: ByteSize(1 << 20),
        }
    }
}

/// `[repo]`: how the CLI treats repositories it has not seen (R1.25).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct RepoSection {
    /// A command given a path in no registered repository registers it
    /// (initializing jj alongside a plain git checkout) and retries once.
    /// Default true; `--no-register` turns it off for one command.
    pub auto_register: bool,
    /// Registration accepts a repository whose `.jj` or `.git` another user
    /// owns (RX.01, S-02). Default false: only the daemon's euid, or under
    /// `sudo` the `SUDO_UID`, as git's `safe.directory` has it. Read by the
    /// daemon.
    pub allow_foreign_owner: bool,
}

impl Default for RepoSection {
    fn default() -> Self {
        Self {
            auto_register: true,
            allow_foreign_owner: false,
        }
    }
}

/// `[store]`: what the daemon does with stores (RX.01, S-03).
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields, default)]
pub struct StoreSection {
    /// `Stores.Build` runs a repository's declared build command. Default
    /// true; false refuses every build with `PermissionDenied`.
    pub allow_builds: bool,
}

impl Default for StoreSection {
    fn default() -> Self {
        Self { allow_builds: true }
    }
}

/// A size in bytes. Read from an integer or a string such as `"1MiB"`
/// ([`parse_byte_size`]); written as a string in the largest exact unit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteSize(pub u64);

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let bytes = self.0;
        for (unit, name) in [(1 << 30, "GiB"), (1 << 20, "MiB"), (1 << 10, "KiB")] {
            if bytes != 0 && bytes.is_multiple_of(unit) {
                return write!(f, "{}{name}", bytes / unit);
            }
        }
        write!(f, "{bytes}B")
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl serde::de::Visitor<'_> for Visitor {
            type Value = ByteSize;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a number of bytes or a size such as \"1MiB\"")
            }

            fn visit_u64<E: serde::de::Error>(self, bytes: u64) -> Result<ByteSize, E> {
                Ok(ByteSize(bytes))
            }

            fn visit_i64<E: serde::de::Error>(self, bytes: i64) -> Result<ByteSize, E> {
                u64::try_from(bytes)
                    .map(ByteSize)
                    .map_err(|_| E::custom("a size cannot be negative"))
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<ByteSize, E> {
                parse_byte_size(text).map(ByteSize).map_err(E::custom)
            }
        }
        deserializer.deserialize_any(Visitor)
    }
}

/// Durations in `config.toml`: read by `humantime-serde`, written in the
/// largest exact unit (`"30m"`, `"24h"`, `"7d"`) so the file stays readable.
mod duration {
    use std::time::Duration;

    use serde::{Deserializer, Serializer};

    pub(super) fn serialize<S: Serializer>(
        value: &Duration,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&super::format_duration(*value))
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Duration, D::Error> {
        humantime_serde::deserialize(deserializer)
    }
}

/// `value` in the largest unit that divides it exactly, as the README writes
/// the defaults: `"500ms"`, `"60s"`, `"30m"`, `"24h"`, `"7d"` (minutes only
/// above 60 seconds, days only above 24 hours). humantime parses every form.
pub fn format_duration(value: Duration) -> String {
    const MS: u128 = 1_000_000;
    const SEC: u128 = 1_000 * MS;
    const MIN: u128 = 60 * SEC;
    const HOUR: u128 = 60 * MIN;
    const DAY: u128 = 24 * HOUR;
    let nanos = value.as_nanos();
    if nanos == 0 {
        return "0s".to_owned();
    }
    if nanos.is_multiple_of(DAY) && nanos > DAY {
        return format!("{}d", nanos / DAY);
    }
    for (unit, name, above) in [
        (HOUR, "h", 0),
        (MIN, "m", MIN),
        (SEC, "s", 0),
        (MS, "ms", 0),
        (1_000, "us", 0),
    ] {
        if nanos.is_multiple_of(unit) && nanos > above {
            return format!("{}{name}", nanos / unit);
        }
    }
    format!("{nanos}ns")
}

/// The host and user jj records on every operation the daemon writes. Not
/// configurable in Phase 1.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OperationIdentity {
    /// The machine's hostname, `operation.hostname`.
    pub hostname: String,
    /// The OS user the daemon runs as, `operation.username`.
    pub username: String,
}

/// Where the state directory and the config file came from, for
/// `reef config show`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Origin {
    /// The layer that chose [`Paths::state_dir`].
    pub state_dir: Source,
    /// The layer that chose [`Paths::config_file`].
    pub config_file: Source,
    /// Whether the config file existed and was read.
    pub config_file_read: bool,
}

/// Fully resolved, validated configuration.
#[derive(Clone, Debug)]
pub struct Config {
    /// Where everything lives.
    pub paths: Paths,
    /// `config.toml` as read, defaults filled in.
    pub file: FileConfig,
    /// The effective `[identity]`: the default email computed.
    pub identity: Identity,
    /// The host and user recorded on operations.
    pub operation: OperationIdentity,
    /// Logging for [`crate::run_main`]: the log file and `log.keep_days`.
    /// The binary adds the filter and format from `REEF_LOG*`;
    /// [`crate::Daemon::start`] installs no subscriber.
    pub log: LogOptions,
    /// The layer each `section.key` came from, for every key in [`KEYS`].
    pub sources: BTreeMap<String, Source>,
    /// Where the state directory and config file came from.
    pub origin: Origin,
    /// Subscription limits (R2.09). Not a configuration-file key and not
    /// in `config show`: set here by tests only.
    pub subscribe: crate::subscribe::Limits,
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
    /// The config file is not valid TOML, or a value has the wrong type.
    #[error("parse the configuration file {}", path.display())]
    Parse {
        /// The file.
        path: PathBuf,
        /// toml's error, with the offending line and a caret under it.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// The config file has a key no section defines.
    #[error("unknown key `{key}` in the configuration file {}", path.display())]
    UnknownKey {
        /// The file.
        path: PathBuf,
        /// The key as `section.key`, or the section alone when it has no
        /// keys.
        key: String,
        /// toml's error, with the offending line and a caret under it.
        #[source]
        source: Box<toml::de::Error>,
    },
    /// A path error [`reef_client::paths`] added after this daemon mapped
    /// its variants.
    #[error(transparent)]
    Paths(PathsError),
    /// A value parsed but breaks a rule, such as `lease.default` above
    /// `lease.max`.
    #[error("invalid {key} in the configuration file {}: {reason}", path.display())]
    Invalid {
        /// The file.
        path: PathBuf,
        /// The key as `section.key`.
        key: String,
        /// The rule it breaks.
        reason: String,
    },
}

impl Classify for ConfigError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::NoHome
            | Self::SocketPathTooLong { .. }
            | Self::Parse { .. }
            | Self::UnknownKey { .. }
            | Self::Invalid { .. } => ErrorKind::InvalidArgument,
            Self::Missing { .. } => ErrorKind::NotFound,
            Self::Paths(error) => error.kind(),
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

impl From<PathsError> for ConfigError {
    fn from(error: PathsError) -> Self {
        match error {
            PathsError::NoHome => Self::NoHome,
            PathsError::SocketPathTooLong { path, max } => Self::SocketPathTooLong { path, max },
            PathsError::Io { path, source } => Self::Io { path, source },
            PathsError::Read { path, source } => Self::Read { path, source },
            PathsError::Missing { path, source_layer } => Self::Missing { path, source_layer },
            PathsError::Parse { path, source } => Self::Parse { path, source },
            // `PathsError` is non-exhaustive: a variant added later keeps its
            // own message and kind until it is given one here.
            other => Self::Paths(other),
        }
    }
}

/// The daemon's [`Paths`] from the settled ones, with the files the
/// client never needs.
fn paths_from(
    layered: Layered,
    daemon: &DaemonSection,
) -> Result<(Paths, Source, Source), ConfigError> {
    let settled = layered.finish(daemon.socket.as_deref(), daemon.pid_file.as_deref())?;
    let state_dir = settled.state_dir;
    let paths = Paths {
        config_file: settled.config_file,
        db: state_dir.join(DB_FILE),
        lock: state_dir.join(LOCK_FILE),
        pid: settled.pid,
        logs: state_dir.join("logs"),
        repos: state_dir.join("repos"),
        socket: settled.socket,
        state_dir,
    };
    Ok((paths, settled.socket_source, settled.pid_source))
}

impl Paths {
    /// Resolves every path from `overrides`, the environment, and the
    /// platform defaults, ignoring `config.toml` (see [`Config::load`] for
    /// the file's `[daemon]` section). Nothing is created.
    pub fn resolve(overrides: &PathOverrides) -> Result<Paths, ConfigError> {
        Self::resolve_with(overrides, |name| std::env::var_os(name), platform_defaults)
    }

    fn resolve_with(
        overrides: &PathOverrides,
        env: impl Fn(&str) -> Option<OsString>,
        defaults: impl FnOnce() -> Result<PlatformDefaults, PathsError>,
    ) -> Result<Paths, ConfigError> {
        let layered = Layered::resolve(overrides, env, defaults)?;
        Ok(paths_from(layered, &DaemonSection::default())?.0)
    }

    /// Paths under `state_dir` alone, ignoring the environment: the socket
    /// is `<state>/daemon.sock` and the config file `<state>/config.toml`.
    /// For tests and tools that run a daemon in a directory of their own.
    pub fn in_state_dir(state_dir: &Path) -> Result<Paths, ConfigError> {
        let overrides = PathOverrides {
            state_dir: Some(state_dir.to_owned()),
            ..PathOverrides::default()
        };
        Self::resolve_with(&overrides, |_| None, || Err(PathsError::NoHome))
    }

    /// The state directory of one repository, `<state>/repos/<repo-id>`.
    pub fn repo_dir(&self, id: RepoId) -> PathBuf {
        self.repos.join(id.to_string())
    }
}

impl Config {
    /// Resolves the paths, reads and validates the config file, and computes
    /// the effective identity. Reads the environment; creates nothing.
    pub fn load(overrides: &PathOverrides) -> Result<Config, ConfigError> {
        Self::load_with(overrides, |name| std::env::var_os(name), platform_defaults)
    }

    fn load_with(
        overrides: &PathOverrides,
        env: impl Fn(&str) -> Option<OsString>,
        defaults: impl FnOnce() -> Result<PlatformDefaults, PathsError>,
    ) -> Result<Config, ConfigError> {
        let layered = Layered::resolve(overrides, env, defaults)?;
        let path = layered.config_file.clone();
        let (file, file_keys, read) = match read_file(&path)? {
            Some((file, keys)) => (file, keys, true),
            // Only the implicit `<state>/config.toml` may be absent: a file
            // named by `REEF_CONFIG` or `--config` that is missing is a typo.
            None if layered.config_file_source != Source::Default => {
                return Err(ConfigError::Missing {
                    path,
                    source_layer: layered.config_file_source,
                });
            }
            None => (FileConfig::default(), BTreeSet::new(), false),
        };
        validate(&file, &path)?;
        let origin = Origin {
            state_dir: layered.state_dir_source,
            config_file: layered.config_file_source,
            config_file_read: read,
        };
        let (paths, socket_source, pid_source) = paths_from(layered, &file.daemon)?;
        let mut sources = default_sources();
        for key in file_keys {
            sources.insert(key, Source::File);
        }
        sources.insert("daemon.socket".to_owned(), socket_source);
        sources.insert("daemon.pid_file".to_owned(), pid_source);
        Ok(Self::build(paths, file, sources, origin))
    }

    /// The default configuration for `paths`, reading no file and no
    /// environment. For tests and tools that run a daemon in a directory of
    /// their own ([`Paths::in_state_dir`]).
    pub fn with_defaults(paths: Paths) -> Config {
        let origin = Origin {
            state_dir: Source::Flag,
            config_file: Source::Default,
            config_file_read: false,
        };
        Self::build(paths, FileConfig::default(), default_sources(), origin)
    }

    fn build(
        paths: Paths,
        file: FileConfig,
        sources: BTreeMap<String, Source>,
        origin: Origin,
    ) -> Config {
        let operation = OperationIdentity {
            hostname: machine_hostname(),
            username: os_username(),
        };
        let identity = Identity {
            name: file.identity.name.clone(),
            email: file
                .identity
                .email
                .clone()
                .unwrap_or_else(|| format!("reef@{}", operation.hostname.to_lowercase())),
        };
        let log = LogOptions {
            file: Some(paths.logs.join(LOG_FILE_PREFIX)),
            keep_days: file.log.keep_days,
            ..LogOptions::default()
        };
        Config {
            paths,
            file,
            identity,
            operation,
            log,
            sources,
            origin,
            subscribe: crate::subscribe::Limits::default(),
        }
    }

    /// The directory holding `repo`'s workspaces: `workspace.root` with
    /// [`REPO_ID_PLACEHOLDER`] replaced by the id, a leading
    /// [`STATE_PLACEHOLDER`] by the state directory, and a relative result
    /// taken relative to the state directory.
    pub fn workspace_root(&self, repo: RepoId) -> PathBuf {
        let root = self
            .file
            .workspace
            .root
            .replace(REPO_ID_PLACEHOLDER, &repo.to_string());
        match root.strip_prefix(STATE_PLACEHOLDER) {
            Some(rest) => self.paths.state_dir.join(rest.trim_start_matches('/')),
            None => self.paths.state_dir.join(root),
        }
    }

    /// Values that are allowed but probably wrong, one sentence each. The
    /// daemon logs them at start; loading runs before logging does.
    pub fn warnings(&self) -> Vec<String> {
        let lease = &self.file.lease;
        let mut warnings = Vec::new();
        if lease.heartbeat >= lease.default / 2 {
            warnings.push(format!(
                "lease.heartbeat ({}) is at least half of lease.default ({}); one missed heartbeat leaves a default lease little time before its grace",
                format_duration(lease.heartbeat),
                format_duration(lease.default)
            ));
        }
        warnings
    }

    /// `watcher.max_concurrent_scans`, defaulting to the number of CPUs.
    pub fn max_concurrent_scans(&self) -> usize {
        self.file
            .watcher
            .max_concurrent_scans
            .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, NonZeroUsize::get))
    }

    /// The jj settings every repository is opened and written with: the
    /// effective identity, `snapshot.max_new_file_size`, the operation host
    /// and user, and `gc.git_executable` as jj's `git.executable-path`.
    pub fn jj_settings(&self) -> Result<UserSettings, SettingsError> {
        self.jj_settings_for(Fsmonitor::None)
    }

    /// [`Config::jj_settings`] for working copies with `fsmonitor`: the
    /// daemon loads Watchman workspaces with [`Fsmonitor::Watchman`]
    /// (R1.14). That is [`SettingsError::FeatureDisabled`] in a build
    /// without the `watchman` feature.
    pub fn jj_settings_for(&self, fsmonitor: Fsmonitor) -> Result<UserSettings, SettingsError> {
        user_settings_from_spec(&SettingsSpec {
            identity: &self.identity,
            max_new_file_size: self.file.snapshot.max_new_file_size.0,
            operation_hostname: &self.operation.hostname,
            operation_username: &self.operation.username,
            fsmonitor,
            git_executable: &self.file.gc.git_executable,
        })
    }

    /// [`Config::file`] with every value computed at load filled in: the
    /// socket and pid file, the identity's email, and the scan limit. What
    /// `reef config show` prints.
    pub fn effective_file(&self) -> FileConfig {
        let mut file = self.file.clone();
        file.daemon.socket = Some(self.paths.socket.clone());
        file.daemon.pid_file = Some(self.paths.pid.clone());
        file.identity.email = Some(self.identity.email.clone());
        file.watcher.max_concurrent_scans = Some(self.max_concurrent_scans());
        file
    }

    /// The effective configuration as TOML that loads back to
    /// [`Config::effective_file`], each key followed by a
    /// `# source: default | file | env | flag` comment. Written by hand,
    /// since the `toml` serializer cannot emit comments.
    pub fn to_effective_toml(&self) -> String {
        let origin = |source: Source, extra: &str| format!("source: {source}{extra}");
        let mut out = String::new();
        let _ = writeln!(out, "# Effective configuration of the Reef daemon.");
        let _ = writeln!(
            out,
            "# state directory: {} ({})",
            self.paths.state_dir.display(),
            origin(self.origin.state_dir, "")
        );
        let _ = writeln!(
            out,
            "# config file: {} ({})",
            self.paths.config_file.display(),
            origin(
                self.origin.config_file,
                if self.origin.config_file_read {
                    ""
                } else {
                    "; not present, defaults apply"
                }
            )
        );
        for (section, entries) in self.effective_entries() {
            let _ = write!(out, "\n[{section}]\n");
            for (key, value) in entries {
                let source = self
                    .sources
                    .get(&format!("{section}.{key}"))
                    .copied()
                    .unwrap_or(Source::Default);
                let _ = writeln!(out, "{key} = {value}  # source: {source}");
            }
        }
        out
    }

    /// Every key of [`Config::effective_file`] as TOML value text, by
    /// section, in [`KEYS`] order.
    #[expect(
        clippy::many_single_char_names,
        reason = "one-letter formatters keep the table one entry per line"
    )]
    #[expect(
        clippy::too_many_lines,
        reason = "one table entry per key, in KEYS order"
    )]
    fn effective_entries(&self) -> Vec<(&'static str, Vec<(&'static str, String)>)> {
        let f = self.effective_file();
        let s = |text: &str| toml::Value::String(text.to_owned()).to_string();
        let p =
            |path: &Option<PathBuf>| s(&path.as_deref().unwrap_or(Path::new("")).to_string_lossy());
        let d = |value: Duration| s(&format_duration(value));
        let n = |value: u64| value.to_string();
        let (l, w) = (&f.lease, &f.watcher);
        vec![
            (
                "daemon",
                vec![
                    ("socket", p(&f.daemon.socket)),
                    ("pid_file", p(&f.daemon.pid_file)),
                ],
            ),
            (
                "identity",
                vec![
                    ("name", s(&f.identity.name)),
                    ("email", s(f.identity.email.as_deref().unwrap_or_default())),
                ],
            ),
            ("log", vec![("keep_days", n(f.log.keep_days.into()))]),
            (
                "lease",
                vec![
                    ("default", d(l.default)),
                    ("max", d(l.max)),
                    ("grace", d(l.grace)),
                    ("restart_grace", d(l.restart_grace)),
                    ("heartbeat", d(l.heartbeat)),
                    ("tick", d(l.tick)),
                ],
            ),
            (
                "watcher",
                vec![
                    ("backend", s(w.backend.as_str())),
                    ("debounce", d(w.debounce)),
                    ("min_interval", d(w.min_interval)),
                    ("fallback_interval", d(w.fallback_interval)),
                    ("watched_fallback_interval", d(w.watched_fallback_interval)),
                    ("narrowed_max_paths", w.narrowed_max_paths.to_string()),
                    (
                        "max_concurrent_scans",
                        w.max_concurrent_scans.unwrap_or_default().to_string(),
                    ),
                    ("watchman_threshold_files", n(w.watchman_threshold_files)),
                    ("watchman_attach_quiet", d(w.watchman_attach_quiet)),
                    ("watchman_attach_defer_max", d(w.watchman_attach_defer_max)),
                    (
                        "notify_attach_background",
                        w.notify_attach_background.to_string(),
                    ),
                ],
            ),
            (
                "workspace",
                vec![
                    ("backend", s(&f.workspace.backend.to_string())),
                    ("root", s(&f.workspace.root)),
                    ("fuse_overlayfs", s(&f.workspace.fuse_overlayfs)),
                    ("overlay_nosuid", f.workspace.overlay_nosuid.to_string()),
                ],
            ),
            (
                "base_cache",
                vec![("max_entries", f.base_cache.max_entries.to_string())],
            ),
            (
                "gc",
                vec![
                    ("undo_window", d(f.gc.undo_window)),
                    ("git_executable", s(&f.gc.git_executable)),
                    ("reindex_limit", n(f.gc.reindex_limit)),
                ],
            ),
            (
                "backup",
                vec![
                    ("retention", d(f.backup.retention)),
                    ("quiesce_max", d(f.backup.quiesce_max)),
                    (
                        "sqlite_pages_per_step",
                        n(f.backup.sqlite_pages_per_step.into()),
                    ),
                ],
            ),
            (
                "snapshot",
                vec![(
                    "max_new_file_size",
                    s(&f.snapshot.max_new_file_size.to_string()),
                )],
            ),
            (
                "repo",
                vec![
                    ("auto_register", f.repo.auto_register.to_string()),
                    (
                        "allow_foreign_owner",
                        f.repo.allow_foreign_owner.to_string(),
                    ),
                ],
            ),
            (
                "store",
                vec![("allow_builds", f.store.allow_builds.to_string())],
            ),
        ]
    }
}

fn default_sources() -> BTreeMap<String, Source> {
    KEYS.iter()
        .flat_map(|(section, keys)| keys.iter().map(move |key| format!("{section}.{key}")))
        .map(|key| (key, Source::Default))
        .collect()
}

/// The file at `path` and the `section.key`s it sets, or `None` when there
/// is no file.
fn read_file(path: &Path) -> Result<Option<(FileConfig, BTreeSet<String>)>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_file(path, &text).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_owned(),
            source,
        }),
    }
}

fn parse_file(path: &Path, text: &str) -> Result<(FileConfig, BTreeSet<String>), ConfigError> {
    let table: toml::Table = text.parse().map_err(|source| ConfigError::Parse {
        path: path.to_owned(),
        source: Box::new(source),
    })?;
    check_byte_size(path, &table)?;
    let file: FileConfig = toml::from_str(text).map_err(|source: toml::de::Error| {
        let source = Box::new(source);
        // serde's message for `deny_unknown_fields`: "unknown field `x`, expected ...".
        match source
            .message()
            .strip_prefix("unknown field `")
            .and_then(|rest| rest.split('`').next())
        {
            Some(name) => ConfigError::UnknownKey {
                path: path.to_owned(),
                key: unknown_key(&table, name),
                source,
            },
            None => ConfigError::Parse {
                path: path.to_owned(),
                source,
            },
        }
    })?;
    let keys = table
        .iter()
        .filter_map(|(section, value)| Some((section, value.as_table()?)))
        .flat_map(|(section, keys)| keys.keys().map(move |key| format!("{section}.{key}")))
        .collect();
    Ok((file, keys))
}

/// The full `section.key` of the unknown `name` toml reported: a key in a
/// known section, or an unknown section with its first key.
fn unknown_key(table: &toml::Table, name: &str) -> String {
    let known = |section: &str| KEYS.iter().find(|(s, _)| *s == section).map(|(_, k)| *k);
    if let Some(value) = table.get(name).filter(|_| known(name).is_none()) {
        return match value.as_table().and_then(|t| t.keys().next()) {
            Some(first) => format!("{name}.{first}"),
            None => name.to_owned(),
        };
    }
    for (section, value) in table {
        if let (Some(keys), Some(entries)) = (known(section), value.as_table())
            && entries.contains_key(name)
            && !keys.contains(&name)
        {
            return format!("{section}.{name}");
        }
    }
    name.to_owned()
}

/// `snapshot.max_new_file_size` must parse as a size; checked before serde so
/// a bad size is [`ConfigError::Invalid`] naming the key.
fn check_byte_size(path: &Path, table: &toml::Table) -> Result<(), ConfigError> {
    let value = table
        .get("snapshot")
        .and_then(toml::Value::as_table)
        .and_then(|snapshot| snapshot.get("max_new_file_size"));
    let reason = match value {
        Some(toml::Value::String(text)) => parse_byte_size(text).err().map(|e| e.to_string()),
        Some(toml::Value::Integer(bytes)) if *bytes < 0 => {
            Some("a size cannot be negative".to_owned())
        }
        _ => None,
    };
    match reason {
        Some(reason) => Err(ConfigError::Invalid {
            path: path.to_owned(),
            key: "snapshot.max_new_file_size".to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

/// The `[lease]` rules of [`validate`], in order: each holds, or names its
/// key and why not.
fn lease_rules(lease: &LeaseSection) -> [(bool, &'static str, String); 8] {
    let fd = |value: Duration| format_duration(value);
    [
        (
            lease.default <= lease.max,
            "lease.default",
            format!(
                "{} is longer than lease.max ({})",
                fd(lease.default),
                fd(lease.max)
            ),
        ),
        (
            lease.grace <= lease.default,
            "lease.grace",
            format!(
                "{} is longer than lease.default ({})",
                fd(lease.grace),
                fd(lease.default)
            ),
        ),
        (
            lease.restart_grace <= lease.max,
            "lease.restart_grace",
            format!(
                "{} is longer than lease.max ({})",
                fd(lease.restart_grace),
                fd(lease.max)
            ),
        ),
        (
            !lease.tick.is_zero(),
            "lease.tick",
            "must be above zero".to_owned(),
        ),
        (
            lease.restart_grace >= lease.tick,
            "lease.restart_grace",
            format!(
                "{} must be at least lease.tick ({}), or a restart could reclaim on its first tick",
                fd(lease.restart_grace),
                fd(lease.tick)
            ),
        ),
        (
            lease.tick < lease.grace,
            "lease.tick",
            format!(
                "{} must be shorter than lease.grace ({})",
                fd(lease.tick),
                fd(lease.grace)
            ),
        ),
        (
            !lease.heartbeat.is_zero(),
            "lease.heartbeat",
            "must be above zero".to_owned(),
        ),
        (
            lease.heartbeat < lease.default,
            "lease.heartbeat",
            format!(
                "{} must be shorter than lease.default ({})",
                fd(lease.heartbeat),
                fd(lease.default)
            ),
        ),
    ]
}

/// The rules values must keep beyond their types.
fn validate(file: &FileConfig, path: &Path) -> Result<(), ConfigError> {
    let fd = |value: Duration| format_duration(value);
    let (lease, watcher) = (&file.lease, &file.watcher);
    let root = &file.workspace.root;
    let path_empty = |path: Option<&PathBuf>| path.is_some_and(|p| p.as_os_str().is_empty());
    let required = [
        (path_empty(file.daemon.socket.as_ref()), "daemon.socket"),
        (path_empty(file.daemon.pid_file.as_ref()), "daemon.pid_file"),
        (file.identity.name.is_empty(), "identity.name"),
        (
            file.identity.email.as_ref().is_some_and(String::is_empty),
            "identity.email",
        ),
        (
            file.workspace.fuse_overlayfs.is_empty(),
            "workspace.fuse_overlayfs",
        ),
        (file.gc.git_executable.is_empty(), "gc.git_executable"),
    ];
    if let Some((_, key)) = required.into_iter().find(|(empty, _)| *empty) {
        return Err(ConfigError::Invalid {
            path: path.to_owned(),
            key: key.to_owned(),
            reason: "must not be empty".to_owned(),
        });
    }
    let rules = lease_rules(lease).into_iter().chain([
        (
            watcher.debounce < watcher.fallback_interval,
            "watcher.debounce",
            format!(
                "{} must be shorter than watcher.fallback_interval ({})",
                fd(watcher.debounce),
                fd(watcher.fallback_interval)
            ),
        ),
        (
            watcher.max_concurrent_scans != Some(0),
            "watcher.max_concurrent_scans",
            "must be at least 1".to_owned(),
        ),
        (
            file.base_cache.max_entries >= 1,
            "base_cache.max_entries",
            "must be at least 1".to_owned(),
        ),
        (
            file.log.keep_days >= 1,
            "log.keep_days",
            "must be at least 1".to_owned(),
        ),
        (
            file.backup.sqlite_pages_per_step >= 1,
            "backup.sqlite_pages_per_step",
            "must be at least 1".to_owned(),
        ),
        (
            root.contains(REPO_ID_PLACEHOLDER)
                && root.rfind(STATE_PLACEHOLDER).is_none_or(|at| at == 0),
            "workspace.root",
            format!(
                "{root:?} must contain {REPO_ID_PLACEHOLDER}, and {STATE_PLACEHOLDER} only at the start"
            ),
        ),
    ]);
    match rules.into_iter().find(|(ok, _, _)| !ok) {
        Some((_, key, reason)) => Err(ConfigError::Invalid {
            path: path.to_owned(),
            key: key.to_owned(),
            reason,
        }),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests;
