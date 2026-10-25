//! Shared by the integration tests: `TestDaemon` from the library's
//! `testing` feature, and helpers that run the `reef-daemon` binary.

#![allow(
    dead_code,
    unused_imports,
    unreachable_pub,
    reason = "each test file uses a different subset"
)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
pub use reef_daemon::testing::{
    StoppedDaemon, SyntheticRepo, TestDaemon, init_synthetic_repo, short_tmp, test_settings,
};
use tempfile::TempDir;

pub mod spawned;

/// How long the binary gets to bind its socket, and to exit after SIGTERM.
pub const BINARY_TIMEOUT: Duration = Duration::from_secs(10);

/// A short temporary directory for a state directory.
pub fn short_tempdir() -> TempDir {
    tempfile::Builder::new()
        .prefix("reef-")
        .tempdir_in(short_tmp())
        .unwrap()
}

/// The `reef-daemon` binary with a clean environment: `REEF_STATE_DIR` set
/// to `state_dir`, the other Reef and logging variables removed, then
/// `vars` applied. Stdout and stderr are piped.
pub fn daemon_command(state_dir: &Path, vars: &[(&str, &str)]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef-daemon"));
    for var in [
        "REEF_SOCKET",
        "REEF_CONFIG",
        "REEF_LOG",
        "RUST_LOG",
        "REEF_LOG_FORMAT",
        "NO_COLOR",
    ] {
        command.env_remove(var);
    }
    command
        .env("REEF_STATE_DIR", state_dir)
        .envs(vars.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// Waits until `socket` exists. False if the child exited first.
pub fn wait_for_socket(child: &mut Child, socket: &Path) -> bool {
    let deadline = Instant::now() + BINARY_TIMEOUT;
    while Instant::now() < deadline {
        if socket.exists() {
            return true;
        }
        if child.try_wait().unwrap().is_some() {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("no socket at {} after {BINARY_TIMEOUT:?}", socket.display());
}

/// Sends SIGTERM and collects the output, failing if the child takes longer
/// than [`BINARY_TIMEOUT`] to exit.
pub fn terminate(child: Child) -> Output {
    kill(
        Pid::from_raw(i32::try_from(child.id()).unwrap()),
        Signal::SIGTERM,
    )
    .unwrap();
    wait_with_timeout(child)
}

/// Collects the output of a child that is exiting by itself.
pub fn wait_with_timeout(mut child: Child) -> Output {
    let deadline = Instant::now() + BINARY_TIMEOUT;
    while child.try_wait().unwrap().is_none() {
        if Instant::now() >= deadline {
            let _ = child.kill();
            panic!("reef-daemon did not exit within {BINARY_TIMEOUT:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    child.wait_with_output().unwrap()
}

/// Runs the binary on a fresh state directory until its socket is bound,
/// then stops it with SIGTERM. Returns the output and the state directory.
/// If the binary exits before binding (bad options), returns that output.
pub fn run_binary(vars: &[(&str, &str)]) -> (Output, TempDir) {
    let tmp = short_tempdir();
    let mut child = daemon_command(tmp.path(), vars).spawn().unwrap();
    let output = if wait_for_socket(&mut child, &tmp.path().join("daemon.sock")) {
        terminate(child)
    } else {
        wait_with_timeout(child)
    };
    (output, tmp)
}

/// The daily log files under `<state>/logs`.
pub fn log_files(state_dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(state_dir.join("logs"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("daemon.log"))
        })
        .collect();
    files.sort();
    files
}

/// The daemon's log lines in this test process, from the one subscriber
/// every test here installs (the first call wins; nextest runs each test in
/// its own process, `cargo test` shares one). `TEST_LOG=reef_daemon=debug`
/// also prints them.
pub fn logs() -> &'static LogCapture {
    static LOGS: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let echo = std::env::var("TEST_LOG").ok();
        let capture = LogCapture(std::sync::Arc::default(), echo.is_some());
        let writer = capture.clone();
        let filter = echo.unwrap_or_else(|| "reef_daemon=info".to_owned());
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .try_init();
        capture
    })
}

/// Captured log text, and whether to echo it; see [`logs`].
#[derive(Clone)]
pub struct LogCapture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>, bool);

impl LogCapture {
    /// Whether one line holds every one of `parts`.
    pub fn has_line(&self, parts: &[&str]) -> bool {
        self.count(parts) > 0
    }

    /// How many lines hold every one of `parts`.
    pub fn count(&self, parts: &[&str]) -> usize {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .filter(|line| parts.iter().all(|part| line.contains(part)))
            .count()
    }
}

impl std::io::Write for LogCapture {
    #[allow(clippy::print_stderr, reason = "TEST_LOG echoes the daemon's logs")]
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        if self.1 {
            eprint!("{}", String::from_utf8_lossy(bytes));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
