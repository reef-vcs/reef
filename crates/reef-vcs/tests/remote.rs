//! `reef` against a daemon's remote listener (R2.08, tests 25 to 30):
//! profiles, commands in remote mode, local-only refusals, the scope rule,
//! the token commands, and `daemon start --serve-remote`.
//!
//! The daemon runs in this process with its listener on a loopback port;
//! `reef` reaches it through a profile in a separate client state
//! directory, or through `REEF_REMOTE` and its companions.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reef_api::v1::RemoteScope;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, short_tmp};
use reef_testing::synth::Preset;
use serde_json::Value;

mod common;

/// `reef` with a clean environment and `state` as its state directory.
fn reef_in(state: &Path) -> Command {
    common::daemon_bin();
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    for var in [
        "REEF_SOCKET",
        "REEF_CONFIG",
        "REEF_LOG",
        "RUST_LOG",
        "REEF_OUTPUT",
        "REEF_WORKSPACE",
        "REEF_AGENT",
        "REEF_REMOTE",
        "REEF_REMOTE_TOKEN",
        "REEF_REMOTE_FINGERPRINT",
    ] {
        command.env_remove(var);
    }
    command.env("REEF_STATE_DIR", state).arg("--no-autostart");
    command
}

/// Runs `command` with `args` and `stdin` off the runtime's worker threads.
async fn run(mut command: Command, args: &[&str], stdin: Option<&str>) -> Output {
    command.args(args);
    if let Some(text) = stdin {
        command.write_stdin(text.to_owned());
    }
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    common::schema::check(args, &output);
    output
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}{}",
        stdout(output),
        stderr(output)
    );
    serde_json::from_str(&stdout(output)).unwrap_or_else(|e| panic!("{e}: {}", stdout(output)))
}

/// The JSON error object of a failed command with exit `code`.
fn failed(output: &Output, code: i32) -> Value {
    assert_eq!(
        output.status.code(),
        Some(code),
        "{}{}",
        stdout(output),
        stderr(output)
    );
    let value: Value = serde_json::from_str(&stdout(output)).unwrap();
    value["error"].clone()
}

fn short_tempdir() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("reef-")
        .tempdir_in(short_tmp())
        .unwrap()
}

/// A daemon with its remote listener on, a registered repository, and
/// what a client needs: the URL and the fingerprint.
async fn remote_daemon() -> (TestDaemon, SyntheticRepo, String, String) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.remote.enabled = true;
        "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
        config.file.remote.names = vec!["localhost".to_owned()];
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
    })
    .await;
    let deadline = Instant::now() + 10 * Duration::from_secs(1);
    let addr = loop {
        if let Some(addr) = daemon.handle.state().remote.bound_addr() {
            break addr;
        }
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let fingerprint = daemon
        .client
        .daemon_status(true)
        .await
        .unwrap()
        .remote
        .unwrap()
        .certificate
        .unwrap()
        .fingerprint;
    (
        daemon,
        repo,
        format!("https://127.0.0.1:{}", addr.port()),
        fingerprint,
    )
}

async fn token(daemon: &TestDaemon, name: &str, scope: RemoteScope) -> String {
    daemon
        .client
        .create_remote_token(name, scope, None)
        .await
        .unwrap()
        .secret
}

/// `reef` on another machine: its own state directory, with no daemon.
fn client(state: &Path) -> Command {
    reef_in(state)
}

// R2.08 25.
#[tokio::test(flavor = "multi_thread")]
async fn remote_add() {
    let (daemon, _repo, url, fingerprint) = remote_daemon().await;
    let secret = token(&daemon, "laptop", RemoteScope::Read).await;
    let tmp = short_tempdir();
    let state = tmp.path().join("client");
    // A wrong pin: refused, with what was presented, and nothing saved.
    let wrong = format!("sha256:{}", "ab".repeat(32));
    let output = run(
        client(&state),
        &[
            "remote",
            "add",
            "vm",
            &url,
            "--fingerprint",
            &wrong,
            "--token-file",
            "-",
        ],
        Some(&secret),
    )
    .await;
    let error = failed(&output, 7);
    assert!(
        error["hint"].as_str().unwrap().contains(&fingerprint),
        "{error}"
    );
    assert!(!state.join("remotes.toml").exists());
    // No pin: the generated certificate is trusted by no root.
    let output = run(
        client(&state),
        &["remote", "add", "vm", &url, "--token-file", "-"],
        Some(&secret),
    )
    .await;
    let error = failed(&output, 7);
    assert!(
        error["hint"].as_str().unwrap().contains(&fingerprint),
        "{error}"
    );
    // The right pin: saved, 0600, the token checked.
    let output = run(
        client(&state),
        &[
            "remote",
            "add",
            "vm",
            &url,
            "--fingerprint",
            &fingerprint,
            "--token-file",
            "-",
        ],
        Some(&format!("{secret}\n")),
    )
    .await;
    let added = json(&output);
    assert_eq!(added["checked"], true);
    let file = state.join("remotes.toml");
    let mode = std::fs::metadata(&file).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let listed = stdout(&run(client(&state), &["-o", "json", "remote", "list"], None).await);
    assert!(listed.contains(&fingerprint), "{listed}");
    assert!(!listed.contains(&secret), "{listed}");
    // A bad token is refused at add.
    let output = run(
        client(&state),
        &[
            "remote",
            "add",
            "bad",
            &url,
            "--fingerprint",
            &fingerprint,
            "--token-file",
            "-",
        ],
        Some("reef_0000000000000000_nope"),
    )
    .await;
    let error = failed(&output, 7);
    assert_eq!(error["reason"], "REMOTE_TOKEN_INVALID", "{error}");
    let output = run(client(&state), &["remote", "remove", "vm"], None).await;
    assert!(output.status.success(), "{}", stderr(&output));
    let output = run(client(&state), &["remote", "remove", "vm"], None).await;
    failed(&output, 3);
}

/// A client state directory with the profile `vm` for `secret`.
async fn profile(url: &str, fingerprint: &str, secret: &str) -> (tempfile::TempDir, PathBuf) {
    let tmp = short_tempdir();
    let state = tmp.path().join("client");
    let output = run(
        client(&state),
        &[
            "remote",
            "add",
            "vm",
            url,
            "--fingerprint",
            fingerprint,
            "--token-file",
            "-",
        ],
        Some(secret),
    )
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    (tmp, state)
}

// R2.08 26.
#[tokio::test(flavor = "multi_thread")]
async fn remote_commands() {
    let (daemon, repo, url, fingerprint) = remote_daemon().await;
    let secret = token(&daemon, "laptop", RemoteScope::Write).await;
    let (_tmp, state) = profile(&url, &fingerprint, &secret).await;
    let remote = |args: &[&str]| {
        let mut all = vec!["--remote", "vm", "-o", "json"];
        all.extend_from_slice(args);
        let all: Vec<String> = all.iter().map(|arg| (*arg).to_owned()).collect();
        let state = state.clone();
        async move {
            let refs: Vec<&str> = all.iter().map(String::as_str).collect();
            run(client(&state), &refs, None).await
        }
    };
    let created = json(&remote(&["ws", "create", "--repo", &repo.id, "--purpose", "remote"]).await);
    let ws = created["id"].as_str().unwrap().to_owned();
    assert!(
        !created["remote_token_id"].as_str().unwrap().is_empty(),
        "{created}"
    );
    let status = json(&remote(&["status", "--workspace", &ws]).await);
    assert_eq!(
        status["workspace_status"]["workspace"]["id"],
        ws.as_str(),
        "{status}"
    );
    json(&remote(&["describe", "--workspace", &ws, "-m", "from afar"]).await);
    let ops = json(&remote(&["op", "log", "--repo", &repo.id, "--limit", "5"]).await);
    let described = ops
        .as_array()
        .unwrap()
        .iter()
        .find(|op| op["kind"] == "describe")
        .unwrap();
    assert_eq!(described["remote"]["token_name"], "laptop", "{described}");
    let listed = json(&remote(&["ws", "list", "--repo", &repo.id]).await);
    assert!(
        listed
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == ws.as_str())
    );
    let status = json(&remote(&["daemon", "status"]).await);
    assert_eq!(status["remote"]["state"], "listening", "{status}");
    // A path names the daemon's machine: refused by the daemon.
    let output = remote(&["ws", "create", "--repo", &repo.id, "--path", "/tmp/x"]).await;
    let error = failed(&output, 7);
    assert_eq!(error["reason"], "REMOTE_ACCESS_DENIED", "{error}");
    json(&remote(&["ws", "release", &ws]).await);
    // The same through the environment, without a profile.
    let mut command = client(&state);
    command
        .env("REEF_REMOTE", &url)
        .env("REEF_REMOTE_TOKEN", &secret)
        .env("REEF_REMOTE_FINGERPRINT", &fingerprint);
    let output = run(
        command,
        &["-o", "json", "ws", "list", "--repo", &repo.id],
        None,
    )
    .await;
    json(&output);
}

// R2.08 27.
#[tokio::test(flavor = "multi_thread")]
async fn local_only_commands() {
    let (daemon, _repo, url, fingerprint) = remote_daemon().await;
    let secret = token(&daemon, "laptop", RemoteScope::Write).await;
    let (_tmp, state) = profile(&url, &fingerprint, &secret).await;
    for args in [
        &["here"][..],
        &["ws", "run", "--", "true"],
        &["ui"],
        &["init", "--check"],
        &["mcp"],
        &["daemon", "start"],
        &["config", "show"],
        &["store", "build", "x"],
        &["remote", "token", "create", "x"],
        &["repo", "register", "/tmp"],
        &["ws", "gc"],
        &["backup", "/tmp/b"],
    ] {
        let mut all = vec!["--remote", "vm", "-o", "json"];
        all.extend_from_slice(args);
        let output = run(client(&state), &all, None).await;
        assert_eq!(
            output.status.code(),
            Some(9),
            "{args:?}: {}",
            stderr(&output)
        );
        // `mcp`'s stdout is the protocol's: its error is on stderr only.
        if args[0] == "mcp" {
            assert!(stderr(&output).contains("remote"), "{}", stderr(&output));
            continue;
        }
        let error = failed(&output, 9);
        assert!(error["hint"].is_string(), "{args:?}: {error}");
    }
    // Nothing started a daemon on this machine.
    assert!(!state.join("daemon.sock").exists());
    assert!(!state.join("daemon.pid").exists());
}

// R2.08 28.
#[tokio::test(flavor = "multi_thread")]
async fn scope_required() {
    let (daemon, _repo, url, fingerprint) = remote_daemon().await;
    let secret = token(&daemon, "laptop", RemoteScope::Read).await;
    let (_tmp, state) = profile(&url, &fingerprint, &secret).await;
    let output = run(
        client(&state),
        &["--remote", "vm", "-o", "json", "status"],
        None,
    )
    .await;
    let error = failed(&output, 2);
    assert_eq!(error["next"][0], "reef", "{error}");
    // `ws create` without `--repo` names no directory on the daemon.
    let output = run(
        client(&state),
        &["--remote", "vm", "-o", "json", "ws", "create"],
        None,
    )
    .await;
    failed(&output, 2);
    // `daemon run` would run a daemon here.
    let output = run(
        client(&state),
        &["--remote", "vm", "-o", "json", "daemon", "run"],
        None,
    )
    .await;
    failed(&output, 9);
    // An unknown profile.
    let output = run(
        client(&state),
        &["--remote", "nope", "-o", "json", "status"],
        None,
    )
    .await;
    failed(&output, 3);
}

// R2.08 29.
#[tokio::test(flavor = "multi_thread")]
async fn token_commands() {
    let (daemon, _repo, url, fingerprint) = remote_daemon().await;
    let on_socket = || reef_in(daemon.state_dir());
    let created = json(
        &run(
            on_socket(),
            &[
                "-o", "json", "remote", "token", "create", "laptop", "--scope", "write",
            ],
            None,
        )
        .await,
    );
    let secret = created["token"].as_str().unwrap().to_owned();
    assert!(secret.starts_with("reef_"), "{created}");
    assert_eq!(created["scope"], "write");
    assert_eq!(created["fingerprint"], fingerprint.as_str());
    assert!(
        created["web_url"]
            .as_str()
            .unwrap()
            .contains("#session=reef_")
    );
    let add: Vec<String> = serde_json::from_value(created["add"].clone()).unwrap();
    assert_eq!(&add[..3], ["reef", "remote", "add"]);
    assert!(add.contains(&fingerprint));
    let text = stdout(
        &run(
            on_socket(),
            &["-o", "text", "remote", "token", "create", "phone"],
            None,
        )
        .await,
    );
    assert!(text.contains("reef remote add"), "{text}");
    let listed = stdout(
        &run(
            on_socket(),
            &["-o", "json", "remote", "token", "list"],
            None,
        )
        .await,
    );
    assert!(!listed.contains(&secret), "{listed}");
    let listed: Value = serde_json::from_str(&listed).unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 2);
    let rotated = json(
        &run(
            on_socket(),
            &["-o", "json", "remote", "token", "rotate", "laptop"],
            None,
        )
        .await,
    );
    assert_ne!(rotated["token"], secret.as_str());
    json(
        &run(
            on_socket(),
            &["-o", "json", "remote", "token", "revoke", "phone"],
            None,
        )
        .await,
    );
    let output = run(
        on_socket(),
        &["-o", "json", "remote", "token", "revoke", "phone"],
        None,
    )
    .await;
    let error = failed(&output, 3);
    assert_eq!(error["reason"], "REMOTE_TOKEN_NOT_FOUND");
    let shown = json(&run(on_socket(), &["-o", "json", "remote", "show"], None).await);
    assert_eq!(shown["certificate"]["fingerprint"], fingerprint.as_str());
    assert_eq!(shown["state"], "listening");
    // `remote show` through the listener reports the same key.
    let (_tmp, state) = profile(&url, &fingerprint, rotated["token"].as_str().unwrap()).await;
    let shown = json(
        &run(
            client(&state),
            &["--remote", "vm", "-o", "json", "remote", "show"],
            None,
        )
        .await,
    );
    assert_eq!(shown["certificate"]["fingerprint"], fingerprint.as_str());
}

/// Stops the daemon a test started in `state` when dropped.
struct StopOnDrop(PathBuf);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.0.join("daemon.pid"))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
        {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
    }
}

/// `reef` that may start a daemon in `state`.
fn reef_starting(state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    command.env_remove("REEF_SOCKET").env_remove("REEF_CONFIG");
    command.env("REEF_STATE_DIR", state);
    command
}

// R2.08 30.
#[tokio::test(flavor = "multi_thread")]
async fn daemon_start_serve_remote() {
    common::daemon_bin();
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        state.join("config.toml"),
        "[remote]\nlisten = \"127.0.0.1:0\"\nnames = [\"localhost\"]\n",
    )
    .unwrap();
    let _guard = StopOnDrop(state.clone());
    let started = run(
        reef_starting(&state),
        &["-o", "json", "daemon", "start", "--serve-remote"],
        None,
    )
    .await;
    let status = json(&started);
    assert_eq!(status["remote"]["state"], "listening", "{status}");
    assert_eq!(status["remote"]["enabled_by"], "flag");
    assert!(stderr(&started).contains("remote listener on"));
    // Again: already running with it.
    json(
        &run(
            reef_starting(&state),
            &["-o", "json", "daemon", "start", "--serve-remote"],
            None,
        )
        .await,
    );
    let stopped = run(reef_starting(&state), &["daemon", "stop"], None).await;
    assert!(stopped.status.success(), "{}", stderr(&stopped));
    // Without it, then asked for it: a conflict naming the stop.
    json(
        &run(
            reef_starting(&state),
            &["-o", "json", "daemon", "start"],
            None,
        )
        .await,
    );
    let output = run(
        reef_starting(&state),
        &["-o", "json", "daemon", "start", "--serve-remote"],
        None,
    )
    .await;
    let error = failed(&output, 5);
    assert_eq!(
        error["next"][error["next"].as_array().unwrap().len() - 1],
        "stop",
        "{error}"
    );
}
