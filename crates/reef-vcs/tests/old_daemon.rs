//! The CLI against a 0.1 daemon (R2.01, tests 15 to 17): `daemon status`
//! and `stop` still work, a busy one is named with exit 9, and an idle one
//! is restarted.
//!
//! The 0.1 daemon is a fake: this test binary run again as its ignored
//! `fake_v0_daemon` test, serving only `reef.daemon.v0.Daemon/Status` from
//! `reef_api::v0` the way 0.1.0 answered it, writing the pid file, and
//! exiting on SIGTERM. Every other v0 or v1 call gets tonic's bare
//! `UNIMPLEMENTED`, as a real 0.1.0 daemon answers a v1 client.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reef_api::v0::daemon_server::{Daemon, DaemonServer};
use reef_api::v0::{StatusRequest, StatusResponse};
use reef_daemon::testing::short_tmp;
use serde_json::Value;
use tempfile::TempDir;
use tonic::{Request, Response, Status};

const SOCKET_ENV: &str = "FAKE_V0_SOCKET";
const STATE_ENV: &str = "FAKE_V0_STATE";
const ACTIVE_ENV: &str = "FAKE_V0_ACTIVE";
/// What the fake reports, as 0.1.0 did.
const FAKE_VERSION: &str = "0.1.0";

/// The fake's `Daemon` service.
struct Fake {
    socket: String,
    state: String,
    active: u32,
}

#[tonic::async_trait]
impl Daemon for Fake {
    async fn status(
        &self,
        _request: Request<StatusRequest>,
    ) -> Result<Response<StatusResponse>, Status> {
        Ok(Response::new(StatusResponse {
            version: FAKE_VERSION.to_owned(),
            pid: std::process::id(),
            uptime_ms: 1,
            state_dir: self.state.clone(),
            socket: self.socket.clone(),
            schema_version: 5,
            repositories: 1,
            backends: Vec::new(),
            active_workspaces: self.active,
        }))
    }
}

/// The fake 0.1 daemon; does nothing unless a test started it.
#[test]
#[ignore = "the fake 0.1 daemon of the old_* tests, which start it"]
fn fake_v0_daemon() {
    let (Ok(socket), Ok(state)) = (std::env::var(SOCKET_ENV), std::env::var(STATE_ENV)) else {
        return;
    };
    let active = std::env::var(ACTIVE_ENV)
        .ok()
        .and_then(|text| text.parse().ok())
        .unwrap_or(0);
    let pid_file = Path::new(&state).join("daemon.pid");
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        std::fs::write(&pid_file, format!("{}\n", std::process::id())).unwrap();
        let mut term =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).unwrap();
        let fake = Fake {
            socket: socket.clone(),
            state: state.clone(),
            active,
        };
        tonic::transport::Server::builder()
            .add_service(DaemonServer::new(fake))
            .serve_with_incoming_shutdown(
                tokio_stream::wrappers::UnixListenerStream::new(listener),
                async move {
                    term.recv().await;
                },
            )
            .await
            .unwrap();
    });
    // As a daemon's last step of shutdown, which `reef daemon stop` waits
    // for.
    let _ = std::fs::remove_file(&pid_file);
    let _ = std::fs::remove_file(&socket);
}

/// A running fake, reaped by a thread so `stop` sees its pid gone; killed
/// on drop.
struct FakeDaemon {
    pid: u32,
    exited: Arc<AtomicBool>,
}

impl FakeDaemon {
    fn spawn(state: &Path, socket: &Path, active: u32) -> FakeDaemon {
        let child: Child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "fake_v0_daemon", "--ignored", "--nocapture"])
            .env(SOCKET_ENV, socket)
            .env(STATE_ENV, state)
            .env(ACTIVE_ENV, active.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let exited = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&exited);
        std::thread::spawn(move || {
            let mut child = child;
            let _ = child.wait();
            flag.store(true, Ordering::SeqCst);
        });
        let deadline = Instant::now() + Duration::from_secs(30);
        while !(socket.exists() && state.join("daemon.pid").exists()) {
            assert!(Instant::now() < deadline, "the fake daemon did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        FakeDaemon { pid, exited }
    }

    fn exited(&self) -> bool {
        self.exited.load(Ordering::SeqCst)
    }

    /// Whether it exits within 15 s.
    fn exits(&self) -> bool {
        let deadline = Instant::now() + Duration::from_secs(15);
        while !self.exited() {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        if !self.exited() {
            let _ = kill(
                Pid::from_raw(i32::try_from(self.pid).unwrap()),
                Signal::SIGTERM,
            );
        }
    }
}

/// A short state directory (for the socket path), and a copy of `reef`
/// with no `reef-daemon` beside it or on `PATH`, so a restarted daemon is
/// this build's `reef daemon run`.
struct Setup {
    tmp: TempDir,
}

impl Setup {
    fn new() -> Setup {
        let tmp = tempfile::Builder::new()
            .prefix("reef-")
            .tempdir_in(short_tmp())
            .unwrap();
        std::fs::create_dir(tmp.path().join("state")).unwrap();
        let bin = tmp.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let exe = bin.join("reef");
        std::fs::copy(env!("CARGO_BIN_EXE_reef"), &exe).unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        Setup { tmp }
    }

    fn state(&self) -> PathBuf {
        self.tmp.path().join("state")
    }

    fn socket(&self) -> PathBuf {
        self.state().join("daemon.sock")
    }

    fn reef(&self, args: &[&str]) -> Output {
        let bin = self.tmp.path().join("bin");
        let mut command = Command::new(bin.join("reef"));
        for var in [
            "REEF_CONFIG",
            "REEF_LOG",
            "RUST_LOG",
            "REEF_OUTPUT",
            "REEF_WORKSPACE",
        ] {
            command.env_remove(var);
        }
        command
            .env("REEF_STATE_DIR", self.state())
            .env("REEF_SOCKET", self.socket())
            .env("PATH", &bin)
            .args(["-o", "json"])
            .args(args);
        command.output().unwrap()
    }
}

impl Drop for Setup {
    /// Stops a daemon a test restarted.
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.state().join("daemon.pid"))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn json(output: &Output) -> Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_slice(&output.stdout).unwrap()
}

/// The JSON error object, after asserting exit 9 and kind `unsupported`.
fn unsupported(output: &Output) -> Value {
    assert_eq!(output.status.code(), Some(9), "{}", stderr(output));
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    let error = value["error"].clone();
    assert_eq!(error["kind"], "unsupported", "{value}");
    assert_eq!(error["reason"], "API_VERSION_MISMATCH", "{value}");
    error
}

// 15
#[test]
fn old_daemon_status_and_stop() {
    let setup = Setup::new();
    let fake = FakeDaemon::spawn(&setup.state(), &setup.socket(), 3);
    let status = json(&setup.reef(&["daemon", "status"]));
    assert_eq!(status["api"], "v0", "{status}");
    assert_eq!(status["api_revision"], Value::Null);
    assert_eq!(status["version"], FAKE_VERSION);
    assert_eq!(status["pid"], fake.pid);
    assert_eq!(status["active_workspaces"], 3);
    let stopped = json(&setup.reef(&["daemon", "stop"]));
    assert_eq!(stopped["pid"], fake.pid);
    assert!(fake.exits(), "the fake is still running after stop");
}

// 16
#[test]
fn old_busy_daemon_is_named() {
    let setup = Setup::new();
    let fake = FakeDaemon::spawn(&setup.state(), &setup.socket(), 2);
    let socket = setup.socket().display().to_string();
    let error = unsupported(&setup.reef(&["--socket", &socket, "ws", "list"]));
    let message = error["message"].as_str().unwrap();
    assert!(message.contains("API v0"), "{message}");
    assert!(
        error["hint"]
            .as_str()
            .unwrap()
            .contains("2 active workspaces"),
        "{error}"
    );
    assert_eq!(
        error["next"],
        serde_json::json!(["reef", "--socket", socket, "daemon", "stop"])
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(!fake.exited(), "a busy daemon must not be stopped");
}

// 17
#[test]
fn old_idle_daemon_is_restarted() {
    let setup = Setup::new();
    let fake = FakeDaemon::spawn(&setup.state(), &setup.socket(), 0);
    let output = setup.reef(&["ws", "list"]);
    assert_eq!(json(&output), Value::Array(Vec::new()));
    let text = stderr(&output);
    assert!(
        text.contains("reef: restarted the daemon (reef 0.1.0, API v0, was running)"),
        "{text}"
    );
    assert!(fake.exits(), "the old daemon is still running");
    let status = json(&setup.reef(&["daemon", "status"]));
    assert_eq!(status["api"], "v1", "{status}");
    assert_eq!(status["version"], env!("CARGO_PKG_VERSION"));
    assert_ne!(status["pid"], fake.pid);
    json(&setup.reef(&["daemon", "stop"]));

    // With --no-autostart the same command is refused instead.
    let setup = Setup::new();
    let fake = FakeDaemon::spawn(&setup.state(), &setup.socket(), 0);
    let error = unsupported(&setup.reef(&["--no-autostart", "ws", "list"]));
    assert_eq!(error["next"], serde_json::json!(["reef", "daemon", "stop"]));
    assert!(!fake.exited());
}

/// `reef daemon start` against a 0.1 daemon applies the same rule: an idle
/// one is restarted, a busy one is named.
#[test]
fn old_daemon_start_restarts_an_idle_one_and_names_a_busy_one() {
    let setup = Setup::new();
    let fake = FakeDaemon::spawn(&setup.state(), &setup.socket(), 0);
    let output = setup.reef(&["daemon", "start"]);
    let status = json(&output);
    assert_eq!(status["api"], "v1", "{status}");
    assert!(
        stderr(&output).contains("reef: restarted the daemon (reef 0.1.0, API v0, was running)"),
        "{}",
        stderr(&output)
    );
    assert!(fake.exits(), "the old daemon is still running");
    json(&setup.reef(&["daemon", "stop"]));

    let setup = Setup::new();
    let fake = FakeDaemon::spawn(&setup.state(), &setup.socket(), 1);
    let error = unsupported(&setup.reef(&["daemon", "start"]));
    assert!(
        error["hint"]
            .as_str()
            .unwrap()
            .contains("1 active workspace;"),
        "{error}"
    );
    std::thread::sleep(Duration::from_millis(200));
    assert!(!fake.exited(), "a busy daemon must not be stopped");
}
