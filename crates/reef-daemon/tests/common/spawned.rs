//! The daemon as a process (R1.23): `reef-daemon` started from its binary on
//! a state directory of its own, killed with SIGKILL, and started again on
//! the same state, with `reef` run against it. The crash, reclaim, and load
//! tests need a pid to kill, so they never use the in-process
//! [`super::TestDaemon`].

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reef_api::v1::IfNotJj;
use reef_daemon::client::Client;
use reef_daemon::config::FileConfig;
use reef_daemon::db::Db;
use reef_testing::synth::Preset;
use serde_json::Value;
use tempfile::TempDir;

use super::{SyntheticRepo, init_synthetic_repo, short_tempdir};

/// How long a spawned daemon gets to answer `Status`: past its own start,
/// which loads every active workspace and reopens every repository.
const START_TIMEOUT: Duration = Duration::from_secs(60);

/// A `reef-daemon` process and a client connected to it. Killed with
/// SIGKILL on drop if still running; on Linux, mounts it left under its
/// directory are unmounted before the directory is removed.
#[derive(Debug)]
pub struct SpawnedDaemon {
    /// The process id.
    pub pid: u32,
    /// The state directory, `<root>/state`.
    pub state_dir: PathBuf,
    /// The socket.
    pub socket: PathBuf,
    /// Connected to this process.
    pub client: Client,
    /// The configuration file it was started with.
    pub config: FileConfig,
    root: PathBuf,
    child: Option<Child>,
    dirs: Dirs,
    /// Set by [`SpawnedDaemon::respawn`], whose new process takes the
    /// mounts over.
    respawned: bool,
}

/// The temporary directories a [`SpawnedDaemon`] owns, passed on by
/// [`SpawnedDaemon::respawn`].
#[derive(Debug)]
struct Dirs {
    /// `root`, when the daemon made it.
    root: Option<TempDir>,
    /// Holds `daemon.stderr`, off the state directory's filesystem, so a
    /// full disk there does not lose the log.
    out: Option<TempDir>,
}

impl SpawnedDaemon {
    /// Starts the binary on a fresh temporary directory with the default
    /// configuration changed by `configure`, and waits until it answers
    /// `Status`.
    pub async fn spawn(configure: impl FnOnce(&mut FileConfig)) -> SpawnedDaemon {
        let tmp = short_tempdir();
        let root = tmp.path().to_owned();
        let mut config = FileConfig::default();
        configure(&mut config);
        Self::start(
            root,
            config,
            Dirs {
                root: Some(tmp),
                out: Some(short_tempdir()),
            },
        )
        .await
    }

    /// [`SpawnedDaemon::spawn`] with the state directory under `root`, which
    /// the caller owns (the disk-full tests' small volume).
    pub async fn spawn_in(root: &Path, configure: impl FnOnce(&mut FileConfig)) -> SpawnedDaemon {
        let mut config = FileConfig::default();
        configure(&mut config);
        Self::start(
            root.to_owned(),
            config,
            Dirs {
                root: None,
                out: Some(short_tempdir()),
            },
        )
        .await
    }

    async fn start(root: PathBuf, config: FileConfig, dirs: Dirs) -> SpawnedDaemon {
        let stderr_path = dirs.out.as_ref().unwrap().path().join("daemon.stderr");
        let state_dir = root.join("state");
        std::fs::create_dir_all(&state_dir).unwrap();
        let config_file = state_dir.join("config.toml");
        std::fs::write(&config_file, toml::to_string(&config).unwrap()).unwrap();
        let socket = state_dir.join("daemon.sock");
        let stderr = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&stderr_path)
            .unwrap();
        let mut command = super::daemon_command(
            &state_dir,
            &[("REEF_CONFIG", config_file.to_str().unwrap())],
        );
        command.stdout(Stdio::null()).stderr(stderr);
        let mut child = command.spawn().unwrap();
        let pid = child.id();
        // A killed daemon leaves its socket file, so the file existing says
        // nothing: wait for this process to answer.
        let begun = Instant::now();
        let client = loop {
            if let Some(status) = child.try_wait().unwrap() {
                panic!(
                    "reef-daemon exited with {status} before answering:\n{}",
                    std::fs::read_to_string(&stderr_path).unwrap_or_default()
                );
            }
            if let Ok(client) = Client::connect(&socket).await
                && client.status().await.is_ok_and(|status| status.pid == pid)
            {
                break client;
            }
            if begun.elapsed() >= START_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                panic!("reef-daemon did not answer within {START_TIMEOUT:?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        };
        SpawnedDaemon {
            pid,
            state_dir,
            socket,
            client,
            config,
            root,
            child: Some(child),
            dirs,
            respawned: false,
        }
    }

    /// The directory holding `state/` and the repositories
    /// [`SpawnedDaemon::register_synth`] generates.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Sends SIGKILL and waits for the process to be gone. Does nothing if
    /// it already is.
    pub fn kill(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = kill(pid(self.pid), Signal::SIGKILL);
            child.wait().unwrap();
        }
    }

    /// Whether the process is still running.
    pub fn is_running(&mut self) -> bool {
        self.child
            .as_mut()
            .is_some_and(|child| child.try_wait().unwrap().is_none())
    }

    /// Starts the binary again on the same state directory and
    /// configuration, killing this process first if it still runs.
    pub async fn respawn(mut self) -> SpawnedDaemon {
        self.kill();
        let root = self.root.clone();
        let config = self.config.clone();
        let dirs = Dirs {
            root: self.dirs.root.take(),
            out: self.dirs.out.take(),
        };
        self.respawned = true;
        drop(self);
        Self::start(root, config, dirs).await
    }

    /// The daemon log's JSON events whose line contains `pattern`, oldest
    /// first. Events still in the logger's buffer when the process was
    /// killed are not there.
    pub fn log_lines(&self, pattern: &str) -> Vec<Value> {
        let logs = self.state_dir.join("logs");
        let Ok(entries) = std::fs::read_dir(&logs) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries.map(|entry| entry.unwrap().path()).collect();
        files.sort();
        files
            .iter()
            .flat_map(|file| json_lines(file, pattern))
            .collect()
    }

    /// [`SpawnedDaemon::log_lines`] from the processes' stderr, which is
    /// the same JSON and is kept off the state directory's filesystem:
    /// every process this daemon was respawned as, not only this one.
    pub fn stderr_lines(&self, pattern: &str) -> Vec<Value> {
        let out = self.dirs.out.as_ref().unwrap();
        json_lines(&out.path().join("daemon.stderr"), pattern)
    }

    /// Generates a `preset` tree at `<root>/<preset>`, makes it a jj
    /// repository snapshotted once, and registers it.
    pub async fn register_synth(&self, preset: Preset) -> SyntheticRepo {
        let root = self.root.join(format!("{preset:?}").to_lowercase());
        let at = root.clone();
        tokio::task::spawn_blocking(move || init_synthetic_repo(&at, preset))
            .await
            .unwrap();
        let registered = self
            .client
            .register_repository(&root, IfNotJj::Fail)
            .await
            .unwrap();
        SyntheticRepo {
            root: std::fs::canonicalize(&root).unwrap(),
            id: registered.repository.unwrap().id,
        }
    }

    /// A second connection to the daemon's database, for what the API does
    /// not show (the reclaim log).
    pub fn db(&self) -> Db {
        Db::open(&self.state_dir.join("reef.sqlite3")).unwrap()
    }
}

impl Drop for SpawnedDaemon {
    fn drop(&mut self) {
        self.kill();
        // A root the caller owns (a volume) is the caller's to unmount.
        #[cfg(target_os = "linux")]
        if !self.respawned {
            unmount_below(&self.root, self.dirs.root.is_some());
        }
    }
}

/// The JSON events in `file` whose line contains `pattern`.
fn json_lines(file: &Path, pattern: &str) -> Vec<Value> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .filter(|line| line.contains(pattern))
        .filter_map(|line| serde_json::from_str(line).ok())
        .collect()
}

/// `id` as a nix pid.
pub fn pid(id: u32) -> Pid {
    Pid::from_raw(i32::try_from(id).unwrap())
}

/// Unmounts everything under `path`, deepest first: the overlay workspaces
/// and store bind mounts a killed daemon leaves behind (R1.18 found that a
/// stopped daemon's directory cannot be removed otherwise).
#[cfg(target_os = "linux")]
pub fn unmount_under(path: &Path) {
    unmount_below(path, true);
}

/// [`unmount_under`], leaving a mount at `path` itself unless `itself`.
#[cfg(target_os = "linux")]
fn unmount_below(path: &Path, itself: bool) {
    let Ok(path) = std::fs::canonicalize(path) else {
        return;
    };
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
    let mut under: Vec<(String, String)> = mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let (point, fstype) = (fields.nth(1)?, fields.next()?);
            (Path::new(point).starts_with(&path) && (itself || Path::new(point) != path))
                .then(|| (point.to_owned(), fstype.to_owned()))
        })
        .collect();
    under.sort_by_key(|(point, _)| std::cmp::Reverse(point.len()));
    for (point, fstype) in under {
        let (program, args) = if fstype.starts_with("fuse") {
            ("fusermount3", ["-uz", point.as_str()])
        } else {
            ("umount", ["-l", point.as_str()])
        };
        // Best effort: a failure leaves the directory for the OS to clean.
        let _ = Command::new(program).args(args).output();
    }
}

/// Kills a process on drop: the agents and their children the tests start.
#[derive(Debug)]
pub struct KillOnDrop(pub Vec<u32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        for id in &self.0 {
            let _ = kill(pid(*id), Signal::SIGKILL);
        }
    }
}

/// The `reef` binary. A workspace-wide build (`cargo nextest run
/// --workspace`, `just test`) puts it beside `reef-daemon`; when it is not
/// there (`-p reef-daemon`), or is older than `reef-daemon` and so may
/// predate a change to the daemon library both link, it is built once, with
/// the daemon library's features and profile as the tests build it, so that a `reef`
/// that was only linked first is relinked and nothing else is rebuilt.
pub fn reef_bin() -> &'static Path {
    static BIN: OnceLock<PathBuf> = OnceLock::new();
    BIN.get_or_init(|| {
        let daemon = Path::new(env!("CARGO_BIN_EXE_reef-daemon"));
        let beside = daemon.with_file_name("reef");
        let modified = |path: &Path| std::fs::metadata(path).and_then(|m| m.modified()).ok();
        if beside.is_file() && modified(&beside) >= modified(daemon) {
            return beside;
        }
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let output = Command::new(cargo)
            .args([
                "build",
                "-p",
                "reef-vcs",
                "--bin",
                "reef",
                "--features",
                if cfg!(feature = "privileged-tests") {
                    "reef-daemon/testing,reef-daemon/privileged-tests"
                } else {
                    "reef-daemon/testing"
                },
                "--message-format=json-render-diagnostics",
            ])
            // The load test runs in release.
            .args((!cfg!(debug_assertions)).then_some("--release"))
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .stderr(Stdio::inherit())
            .output()
            .unwrap();
        assert!(output.status.success(), "cargo build -p reef-vcs failed");
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|message| {
                message["reason"] == "compiler-artifact" && message["target"]["name"] == "reef"
            })
            .and_then(|message| message["executable"].as_str().map(PathBuf::from))
            .expect("cargo reported no `reef` executable")
    })
}

/// `reef --no-autostart` against `daemon`, with a clean environment and
/// stdin from `/dev/null`.
pub fn reef(daemon: &SpawnedDaemon) -> Command {
    let mut command = Command::new(reef_bin());
    for var in [
        "REEF_SOCKET",
        "REEF_CONFIG",
        "REEF_LOG",
        "RUST_LOG",
        "REEF_LOG_FORMAT",
    ] {
        command.env_remove(var);
    }
    command
        .env("REEF_STATE_DIR", &daemon.state_dir)
        .arg("--no-autostart")
        .stdin(Stdio::null());
    command
}

/// `reef ws run --repo <repo> <args>`, started with stdout and stderr
/// piped.
pub fn ws_run(daemon: &SpawnedDaemon, repo: &str, args: &[&str]) -> Child {
    reef(daemon)
        .args(["ws", "run", "--repo", repo])
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Free bytes for an unprivileged user on the filesystem holding `path`.
#[allow(
    clippy::useless_conversion,
    reason = "statvfs's field types differ between macOS and Linux"
)]
pub fn free_bytes(path: &Path) -> u64 {
    let stat = nix::sys::statvfs::statvfs(path).unwrap();
    u64::from(stat.blocks_available()) * u64::from(stat.fragment_size())
}

/// The names under `ws_root` that are workspace ids.
pub fn workspace_dirs(ws_root: &Path) -> std::collections::BTreeSet<String> {
    std::fs::read_dir(ws_root)
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
                .filter(|name| name.parse::<reef_daemon::model::WorkspaceId>().is_ok())
                .collect()
        })
        .unwrap_or_default()
}

/// Each of `paths`' bytes in `commit`'s tree, read through the registered
/// `root`'s jj repository off the runtime; `None` where absent.
pub async fn files_at(root: &Path, commit: &str, paths: &[String]) -> Vec<Option<Vec<u8>>> {
    let (root, paths) = (root.to_owned(), paths.to_vec());
    let commit: reef_lib::jj::CommitIdHex = commit.parse().unwrap();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let repo = reef_lib::jj::Repo::open(&root, &super::test_settings())
                .await
                .unwrap();
            let mut files = Vec::new();
            for path in &paths {
                let path = jj_lib::repo_path::RepoPathBuf::from_internal_string(path).unwrap();
                files.push(repo.read_file_at(&commit, &path).await.unwrap());
            }
            files
        })
    })
    .await
    .unwrap()
}

/// `path`'s bytes in `commit`'s tree, `None` if absent.
pub async fn file_at(root: &Path, commit: &str, path: &str) -> Option<Vec<u8>> {
    files_at(root, commit, &[path.to_owned()]).await.remove(0)
}
