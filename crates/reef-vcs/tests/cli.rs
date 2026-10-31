//! The `reef` CLI against a daemon (R1.22): output modes, exit codes,
//! autostart, and `daemon start|stop|status`. Tests 1 to 11 are the brief's;
//! R1.17 adds `ws_gc_dry_run`, R1.20 the store tests, R1.25 the implicit
//! registration tests, and R1.26 the agent contract tests at the end.
//!
//! Most tests run a [`TestDaemon`] in this process and the `reef` binary
//! against its socket with `--no-autostart`, so a mistake never starts a
//! second daemon. Stdout is piped, so JSON is the default output.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use assert_cmd::assert::OutputAssertExt as _;
use nix::errno::Errno;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reef_daemon::model::WorkspaceState;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, init_synthetic_repo, short_tmp};
use reef_testing::synth::Preset;
use serde_json::Value;
use tempfile::TempDir;

mod common;

/// `reef` with a clean environment and the given state directory, and this
/// build's `reef-daemon` beside it for the commands that run it.
fn reef_in(state_dir: &Path) -> Command {
    common::daemon_bin();
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    for var in [
        "REEF_SOCKET",
        "REEF_CONFIG",
        "REEF_LOG",
        "RUST_LOG",
        "REEF_LOG_FORMAT",
        "REEF_OUTPUT",
        "REEF_WORKSPACE",
        "REEF_AGENT",
    ] {
        command.env_remove(var);
    }
    command.env("REEF_STATE_DIR", state_dir);
    command
}

/// `reef --no-autostart` against `daemon`.
fn reef(daemon: &TestDaemon) -> Command {
    let mut command = reef_in(daemon.state_dir());
    command.arg("--no-autostart");
    command
}

/// Runs `command` with `args` off the runtime's worker threads, which the
/// in-process daemon needs.
async fn run(mut command: Command, args: &[&str]) -> Output {
    command.args(args);
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// Stdout as exactly one JSON value, after asserting success.
fn json(output: &Output) -> Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_str(&stdout(output)).unwrap_or_else(|e| panic!("{e}: {}", stdout(output)))
}

/// Asserts the exit code and that stdout is exactly one JSON error object
/// of `kind`. Returns stderr.
fn failed(output: &Output, code: i32, kind: &str) -> String {
    output.clone().assert().code(code);
    let value: Value = serde_json::from_str(&stdout(output))
        .unwrap_or_else(|e| panic!("{e}: {:?}", stdout(output)));
    assert_eq!(value["error"]["kind"], kind, "{value}");
    assert!(value["error"]["message"].is_string(), "{value}");
    let text = stderr(output);
    assert!(text.starts_with("error: "), "{text}");
    text
}

async fn with_repo() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo(Preset::Small).await
}

/// The first entry of a directory, by name, that is not hidden and is a
/// directory if `dir` is set.
fn first_entry(root: &Path, dir: bool) -> PathBuf {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| !path.file_name().unwrap().to_str().unwrap().starts_with('.'))
        .filter(|path| path.is_dir() == dir)
        .collect();
    entries.sort();
    entries.into_iter().next().unwrap()
}

async fn create(daemon: &TestDaemon, repo: &SyntheticRepo) -> Value {
    json(&run(reef(daemon), &["ws", "create", "--repo", &repo.id]).await)
}

/// R1.28: `reef --version` names the commit and target, so a bug report
/// names the exact binary. The commit is `unknown` outside a git checkout
/// (a source tarball, a Reef workspace).
#[test]
fn version_names_commit_and_target() {
    let tmp = tempfile::tempdir().unwrap();
    let output = reef_in(tmp.path()).arg("--version").output().unwrap();
    assert!(output.status.success());
    let text = stdout(&output);
    let rest = text
        .trim_end()
        .strip_prefix(concat!("reef ", env!("CARGO_PKG_VERSION"), " ("))
        .and_then(|rest| rest.strip_suffix(')'))
        .unwrap_or_else(|| panic!("unexpected --version: {text}"));
    let (commit, target) = rest.split_once(", ").unwrap();
    assert!(
        commit == "unknown"
            || (commit.len() == 12 && commit.bytes().all(|byte| byte.is_ascii_hexdigit())),
        "{commit}"
    );
    assert!(target.starts_with(std::env::consts::ARCH), "{target}");
    assert!(target.contains(match std::env::consts::OS {
        "macos" => "apple-darwin",
        os => os,
    }));
}

// 1
#[test]
fn help_lists_every_command() {
    let tmp = tempfile::tempdir().unwrap();
    let output = reef_in(tmp.path()).arg("--help").output().unwrap();
    let text = stdout(&output);
    for command in [
        "ws",
        "here",
        "status",
        "log",
        "show",
        "diff",
        "describe",
        "abandon",
        "undo",
        "op",
        "file",
        "guide",
        "init",
        "mcp",
        "daemon",
        "config",
        "repo",
        "store",
        "backup",
        "restore",
        "completions",
    ] {
        assert!(
            text.lines()
                .any(|line| line.trim_start().starts_with(&format!("{command} "))),
            "{command}: {text}"
        );
    }
    let output = reef_in(tmp.path()).args(["ws", "--help"]).output().unwrap();
    let text = stdout(&output);
    for command in [
        "create", "list", "inspect", "snapshot", "release", "move", "run", "gc",
    ] {
        assert!(
            text.lines()
                .any(|line| line.trim_start().starts_with(&format!("{command} "))),
            "{command}: {text}"
        );
    }
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn brief_json_when_piped() {
    let (daemon, repo) = with_repo().await;
    let output = run(reef(&daemon), &["repo", "list"]).await;
    let value = json(&output);
    let list = value.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], repo.id.as_str());
    // A command without a view: its full JSON, compact (ADR 0045).
    assert_eq!(stdout(&output).lines().count(), 3, "{}", stdout(&output));
    assert!(list[0]["root"].is_string(), "{value}");

    // `ws list`: one compact element per line, with only the keys an agent
    // acts on.
    let created = create(&daemon, &repo).await;
    let output = run(reef(&daemon), &["ws", "list"]).await;
    let text = stdout(&output);
    assert_eq!(text.lines().collect::<Vec<_>>().len(), 3, "{text}");
    let listed = json(&output);
    let keys: Vec<&String> = listed[0].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["changed_file_count", "id", "owner"], "{listed}");
    assert_eq!(listed[0]["id"], created["id"]);
    let full = run(reef(&daemon), &["-o", "json", "ws", "list"]).await;
    let full_value = json(&full);
    for key in ["path", "lease", "changed_file_count"] {
        assert!(full_value[0].get(key).is_some(), "{key}: {full_value}");
    }
    assert!(stdout(&full).contains("\n    \"path\": "), "pretty");
    // The same full JSON, but for what the watcher changes in between.
    let mut env = reef(&daemon);
    env.env("REEF_OUTPUT", "json");
    let steady = |mut value: Value| {
        for ws in value.as_array_mut().unwrap() {
            let ws = ws.as_object_mut().unwrap();
            for key in [
                "watch_state",
                "last_snapshot_at",
                "last_snapshot_op",
                "commit_id",
            ] {
                ws.remove(key);
            }
        }
        value
    };
    assert_eq!(
        steady(json(&run(env, &["ws", "list"]).await)),
        steady(full_value.clone())
    );
    let mut env = reef(&daemon);
    env.env("REEF_OUTPUT", "json");
    assert_eq!(
        stdout(&run(env, &["-o", "brief", "ws", "list"]).await),
        text
    );

    let output = run(reef(&daemon), &["-o", "text", "repo", "list"]).await;
    output.clone().assert().success();
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text}");
    assert!(lines[0].starts_with("ID  "), "{text}");
    assert!(lines[0].contains("ROOT"), "{text}");
    assert!(lines[1].starts_with(&repo.id), "{text}");
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn repo_register_and_list() {
    let daemon = TestDaemon::start().await;
    let root = daemon.tmp.path().join("repo");
    let at = root.clone();
    tokio::task::spawn_blocking(move || init_synthetic_repo(&at, Preset::Small))
        .await
        .unwrap();
    let root = root.canonicalize().unwrap();

    let registered = json(&run(reef(&daemon), &["repo", "register", root.to_str().unwrap()]).await);
    let id = registered["id"].as_str().unwrap().to_owned();
    assert_eq!(id.len(), 16);
    assert_eq!(registered["root"], root.to_str().unwrap());
    assert_eq!(registered["colocated"], false);

    let listed = json(&run(reef(&daemon), &["repo", "list"]).await);
    assert_eq!(listed[0]["id"], id.as_str());
    assert_eq!(listed[0]["colocated"], false);

    // From inside the repository, by the default path `.`.
    let mut again = reef(&daemon);
    again.current_dir(first_entry(&root, true));
    let output = run(again, &["repo", "register"]).await;
    let text = failed(&output, 4, "already_exists");
    assert!(text.contains(&id), "{text}");
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn ws_create_prints_path() {
    let (daemon, repo) = with_repo().await;
    let output = run(
        reef(&daemon),
        &["-o", "text", "ws", "create", "--repo", &repo.id],
    )
    .await;
    output.clone().assert().success();
    let text = stdout(&output);
    let path = text.strip_suffix('\n').unwrap();
    assert!(!path.contains('\n'), "{text:?}");
    assert!(Path::new(path).is_absolute(), "{text:?}");
    let file = first_entry(&repo.root, false);
    assert_eq!(
        std::fs::read(Path::new(path).join(file.file_name().unwrap())).unwrap(),
        std::fs::read(&file).unwrap()
    );

    // JSON, and the repository taken from the current directory.
    let mut from_root = reef(&daemon);
    from_root.current_dir(&repo.root);
    let value = json(&run(from_root, &["ws", "create", "--purpose", "cli test"]).await);
    assert_eq!(value["id"].as_str().unwrap().len(), 16);
    assert_eq!(value["repo_id"], repo.id.as_str());
    assert!(Path::new(value["path"].as_str().unwrap()).is_dir());
    assert_eq!(value["change_id"].as_str().unwrap().len(), 32);
    assert_eq!(value["purpose"], "cli test");
    assert!(
        value["owner"].as_str().unwrap().contains('@'),
        "the default owner is <user>@<host>: {value}"
    );
    let expires = value["lease"]["expires_at"].as_str().unwrap();
    assert!(expires.ends_with('Z'), "RFC 3339 UTC: {expires}");
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn ws_list_and_inspect() {
    let (daemon, repo) = with_repo().await;
    let created = create(&daemon, &repo).await;
    let id = created["id"].as_str().unwrap().to_owned();

    let listed = json(&run(reef(&daemon), &["-o", "json", "ws", "list"]).await);
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["state"], "active");

    let output = run(reef(&daemon), &["-o", "text", "ws", "list"]).await;
    let text = stdout(&output);
    let header = text.lines().next().unwrap();
    let columns: Vec<&str> = header.split_whitespace().collect();
    assert_eq!(
        columns,
        [
            "ID",
            "STATE",
            "CHANGE",
            "FILES",
            "OWNER",
            "LEASE",
            "BACKEND",
            "ATTENTION",
            "PATH"
        ]
    );
    assert!(text.contains(" left "), "a relative lease: {text}");

    let inspected = json(&run(reef(&daemon), &["ws", "inspect", &id[..4]]).await);
    assert_eq!(inspected["id"], id.as_str());
    assert!(
        inspected["disk_usage"]["files_changed"].is_u64(),
        "{inspected}"
    );
    // R1.16: the snapshot index's counts and recent list.
    assert!(inspected["snapshot_count"].is_u64(), "{inspected}");
    assert!(inspected["local_snapshot_count"].is_u64(), "{inspected}");
    assert!(inspected["recent_snapshots"].is_array(), "{inspected}");
    let output = run(reef(&daemon), &["-o", "text", "ws", "inspect", &id]).await;
    assert!(stdout(&output).starts_with("id:"), "{}", stdout(&output));
    assert!(
        stdout(&output).contains("\nsnapshots:"),
        "{}",
        stdout(&output)
    );

    // A hex prefix nothing has: 3. Not hex at all: 2.
    let first = if id.starts_with('f') { '0' } else { 'f' };
    let missing = format!("{first}{}", &id[1..4]);
    failed(
        &run(reef(&daemon), &["ws", "inspect", &missing]).await,
        3,
        "not_found",
    );
    failed(
        &run(reef(&daemon), &["ws", "inspect", "zzzz"]).await,
        2,
        "invalid_argument",
    );

    // A second row sharing the first four characters: 2, naming both.
    let state = daemon.handle.state();
    let mut twin = state.db.workspace(id.parse().unwrap()).unwrap();
    let suffix = if &id[4..] == "000000000000" {
        "ffffffffffff"
    } else {
        "000000000000"
    };
    let twin_id = format!("{}{suffix}", &id[..4]);
    twin.id = twin_id.parse().unwrap();
    twin.path = PathBuf::from(format!("/nonexistent/{twin_id}"));
    twin.state_dir = PathBuf::from(format!("/nonexistent/{twin_id}.state"));
    twin.state = WorkspaceState::Released;
    twin.lease_id = None;
    state.db.insert_workspace(&twin).unwrap();
    let text = failed(
        &run(reef(&daemon), &["ws", "inspect", &id[..4]]).await,
        2,
        "invalid_argument",
    );
    assert!(text.contains("ambiguous"), "{text}");
    assert!(text.contains(&id) && text.contains(&twin_id), "{text}");
    // R1.26: a hint, and no command to run, since only the agent can pick.
    assert!(text.contains("\nhint: Give more of the id: "), "{text}");
    assert!(!text.contains("\nnext: "), "{text}");
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn ws_release() {
    let (daemon, repo) = with_repo().await;
    let created = create(&daemon, &repo).await;
    let id = created["id"].as_str().unwrap();
    let path = created["path"].as_str().unwrap();

    let output = run(reef(&daemon), &["-o", "text", "ws", "release", &id[..6]]).await;
    output.clone().assert().success();
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), format!("released {id}\n"));
    assert!(!Path::new(path).exists());

    assert_eq!(
        json(&run(reef(&daemon), &["ws", "list"]).await),
        Value::Array(vec![])
    );
    let all = json(&run(reef(&daemon), &["-o", "json", "ws", "list", "--all"]).await);
    assert_eq!(all[0]["id"], id);
    assert_eq!(all[0]["state"], "released");
    assert_eq!(all[0]["lease"]["state"], "ended");
    assert_eq!(all[0]["lease"]["end_reason"], "released");

    // JSON mode prints the release's result instead of the note.
    let second = create(&daemon, &repo).await;
    let second_id = second["id"].as_str().unwrap();
    let output = run(reef(&daemon), &["ws", "release", second_id]).await;
    let value = json(&output);
    assert_eq!(value["id"], second_id);
    assert_eq!(value["snapshot_taken"], true);
    assert_eq!(stderr(&output), "");
}

/// `ws move` (R1.06): a new change on the workspace's own, after a final
/// snapshot that holds its edit; same id and path, the path printed in
/// text mode.
#[tokio::test(flavor = "multi_thread")]
async fn ws_move() {
    let (daemon, repo) = with_repo().await;
    let created = create(&daemon, &repo).await;
    let id = created["id"].as_str().unwrap();
    let path = created["path"].as_str().unwrap();
    let change = created["change_id"].as_str().unwrap();
    std::fs::write(Path::new(path).join("edit.txt"), "kept\n").unwrap();

    let value = json(&run(reef(&daemon), &["ws", "move", path, "--new-on", change]).await);
    assert_eq!(value["id"], id);
    assert_eq!(value["path"], path);
    assert_ne!(value["change_id"], change);
    assert!(value["final_op_id"].is_string(), "{value}");
    assert!(value["op_id"].is_string(), "{value}");
    assert_eq!(value["files_added"], 0, "the same tree: {value}");
    assert!(
        Path::new(path).join("edit.txt").is_file(),
        "on top of the edit"
    );

    let output = run(
        reef(&daemon),
        &["-o", "text", "ws", "move", &id[..6], "--change", change],
    )
    .await;
    output.clone().assert().success();
    assert_eq!(stdout(&output), format!("{path}\n"));

    failed(
        &run(reef(&daemon), &["ws", "move", id]).await,
        2,
        "invalid_argument",
    );
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn exit_codes() {
    let (daemon, repo) = with_repo().await;
    let created = create(&daemon, &repo).await;
    let id = created["id"].as_str().unwrap();
    json(&run(reef(&daemon), &["ws", "release", id]).await);
    failed(
        &run(reef(&daemon), &["ws", "release", id]).await,
        5,
        "conflict",
    );

    let output = run(
        reef(&daemon),
        &["--socket", "/nonexistent/reef.sock", "repo", "list"],
    )
    .await;
    let text = failed(&output, 6, "unavailable");
    assert!(text.contains("daemon is not running"), "{text}");
    // R1.26: the fix, with the flag that chose the socket.
    assert!(
        text.contains("\nnext: reef --socket /nonexistent/reef.sock daemon start\n"),
        "{text}"
    );
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(
        value["error"]["next"],
        serde_json::json!([
            "reef",
            "--socket",
            "/nonexistent/reef.sock",
            "daemon",
            "start"
        ])
    );
    assert!(value["error"]["hint"].is_string(), "{value}");

    let text = failed(
        &run(reef(&daemon), &["ws", "create", "--change"]).await,
        2,
        "invalid_argument",
    );
    assert!(text.contains("--change"), "clap's own message: {text}");
    // A missing subcommand: clap prints help, the JSON names the problem.
    let output = run(reef(&daemon), &["ws"]).await;
    output.clone().assert().code(2);
    assert!(stderr(&output).contains("Usage:"), "{}", stderr(&output));
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["error"]["kind"], "invalid_argument");
    assert_eq!(
        value["error"]["message"],
        "a subcommand is required; see --help"
    );
    assert_eq!(value["error"]["hint"], "Usage: reef ws [OPTIONS] <COMMAND>");
    // A guess at "where am I" points at `reef here` (RX.07), in both modes;
    // the same word under a noun keeps the usage line. (`status` became a
    // command in R2.27.)
    let output = run(reef(&daemon), &["info"]).await;
    let text = stderr(&output);
    output.clone().assert().code(2);
    assert!(text.contains("\nnext: reef here\n"), "{text}");
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["error"]["kind"], "invalid_argument");
    assert_eq!(value["error"]["message"], "unrecognized subcommand 'info'");
    assert_eq!(
        value["error"]["hint"],
        "Reef has no `info`; `reef here` shows the workspace you are in and its change."
    );
    assert_eq!(value["error"]["next"], serde_json::json!(["reef", "here"]));
    let output = run(
        reef(&daemon),
        &["-o", "text", "--state-dir", "/nonexistent/state", "whoami"],
    )
    .await;
    output.clone().assert().code(2);
    assert_eq!(stdout(&output), "");
    let text = stderr(&output);
    assert!(
        text.contains("\nhint: Reef has no `whoami`; `reef here`"),
        "{text}"
    );
    // `next` asks the daemon this command line named.
    assert!(
        text.ends_with("\nnext: reef --state-dir /nonexistent/state here\n"),
        "{text}"
    );
    let output = run(reef(&daemon), &["ws", "info"]).await;
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["error"]["hint"], "Usage: reef ws [OPTIONS] <COMMAND>");
    assert!(value["error"].get("next").is_none(), "{value}");
    // `-o text` keeps stdout empty on a usage error.
    let output = run(reef(&daemon), &["-o", "text", "ws", "create", "--change"]).await;
    output.clone().assert().code(2);
    assert_eq!(stdout(&output), "");
}

/// SIGTERMs the daemon named by a state directory's pid file on drop, so a
/// failing autostart test leaves nothing running.
struct StopOnDrop(PathBuf);

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(pid) = std::fs::read_to_string(self.0.join("daemon.pid"))
            .ok()
            .and_then(|text| text.trim().parse::<i32>().ok())
            .filter(|pid| *pid != i32::try_from(std::process::id()).unwrap())
        {
            let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
        }
    }
}

/// A short temporary directory, for socket paths.
fn short_tempdir() -> TempDir {
    tempfile::Builder::new()
        .prefix("reef-")
        .tempdir_in(short_tmp())
        .unwrap()
}

// 8
#[test]
fn autostart_spawns_daemon() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    // A copy of `reef` with this build's `reef-daemon` beside it and
    // nothing on PATH, so the daemon started is never a stale `reef-daemon`
    // from another build.
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let exe = bin.join("reef");
    std::fs::copy(env!("CARGO_BIN_EXE_reef"), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    common::daemon_beside(&bin);
    let _guard = StopOnDrop(state.clone());
    let reef = || {
        let mut command = Command::new(&exe);
        for var in [
            "REEF_SOCKET",
            "REEF_CONFIG",
            "REEF_LOG",
            "RUST_LOG",
            "REEF_AGENT",
        ] {
            command.env_remove(var);
        }
        command.env("REEF_STATE_DIR", &state).env("PATH", &bin);
        command
    };

    let output = reef().args(["repo", "list"]).output().unwrap();
    assert_eq!(json(&output), Value::Array(vec![]));
    let text = stderr(&output);
    let pid: u32 = text
        .strip_prefix("reef: started daemon (pid ")
        .and_then(|rest| rest.strip_suffix(")\n"))
        .unwrap_or_else(|| panic!("{text:?}"))
        .parse()
        .unwrap();
    let pid_file = state.join("daemon.pid");
    assert_eq!(
        std::fs::read_to_string(&pid_file).unwrap().trim(),
        pid.to_string()
    );

    let status = json(&reef().args(["daemon", "status"]).output().unwrap());
    assert_eq!(status["pid"], pid);
    assert_eq!(status["repositories"], 0);
    let output = reef().args(["daemon", "start"]).output().unwrap();
    assert_eq!(json(&output)["pid"], pid);
    assert!(
        stderr(&output).contains("already running"),
        "{}",
        stderr(&output)
    );

    // A pid file that disagrees with the daemon: refused, nothing signalled.
    std::fs::write(&pid_file, "1\n").unwrap();
    let text = failed(
        &reef().args(["daemon", "stop"]).output().unwrap(),
        5,
        "conflict",
    );
    assert!(text.contains("not signalling"), "{text}");
    std::fs::write(&pid_file, format!("{pid}\n")).unwrap();

    // Stopped through `--socket` from a CLI on another state directory: the
    // daemon's own pid file, in the state directory it reports, agrees.
    let elsewhere = tmp.path().join("elsewhere");
    let socket = status["socket"].as_str().unwrap().to_owned();
    let started = Instant::now();
    let output = reef()
        .env("REEF_STATE_DIR", &elsewhere)
        .args(["-o", "text", "--socket", &socket, "daemon", "stop"])
        .output()
        .unwrap();
    output.clone().assert().success();
    assert!(started.elapsed() < Duration::from_secs(15));
    assert_eq!(stderr(&output), format!("stopped daemon (pid {pid})\n"));
    assert!(!pid_file.exists());
    // `stop` returns once the pid file is gone and the socket refuses, the
    // last step of shutdown; the process may take a moment more to exit and
    // be reaped, longer on a loaded runner.
    let pid = Pid::from_raw(i32::try_from(pid).unwrap());
    let deadline = Instant::now() + Duration::from_secs(10);
    while kill(pid, None) != Err(Errno::ESRCH) {
        assert!(
            Instant::now() < deadline,
            "pid {pid} still exists 10 s after stop"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = reef().args(["daemon", "status"]).output().unwrap();
    failed(&output, 6, "unavailable");
}

// 9
#[test]
fn daemon_stop_refuses_stale_pid() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    std::fs::create_dir(&state).unwrap();
    std::fs::write(
        state.join("daemon.pid"),
        format!("{}\n", std::process::id()),
    )
    .unwrap();
    let output = reef_in(&state).args(["daemon", "stop"]).output().unwrap();
    let text = failed(&output, 3, "not_found");
    assert!(text.contains("no daemon answering"), "{text}");
    assert!(text.contains("was not signalled"), "{text}");
    // Still here, so not signalled.
}

// 10
#[test]
fn completions_generate() {
    let tmp = tempfile::tempdir().unwrap();
    let output = reef_in(tmp.path())
        .args(["completions", "bash"])
        .output()
        .unwrap();
    output.clone().assert().success();
    assert!(stdout(&output).contains("reef"));
    let output = reef_in(tmp.path())
        .args(["completions", "zsh"])
        .output()
        .unwrap();
    assert!(stdout(&output).contains("#compdef reef"));
}

/// A copy of `reef` in `<tmp>/bin`, with nothing beside it.
fn lone_reef(tmp: &Path) -> PathBuf {
    let bin = tmp.join("bin");
    std::fs::create_dir(&bin).unwrap();
    let exe = bin.join("reef");
    std::fs::copy(env!("CARGO_BIN_EXE_reef"), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    exe
}

/// `exe` with a clean environment, `PATH` its own directory only.
fn lone_command(exe: &Path, state: &Path) -> std::process::Command {
    let mut command = std::process::Command::new(exe);
    for var in [
        "REEF_SOCKET",
        "REEF_CONFIG",
        "REEF_LOG",
        "RUST_LOG",
        "REEF_OUTPUT",
        "REEF_AGENT",
    ] {
        command.env_remove(var);
    }
    command
        .env("REEF_STATE_DIR", state)
        .env("PATH", exe.parent().unwrap());
    command
}

/// R2.05 test 3: `reef daemon run` is an `exec` of `reef-daemon
/// --foreground` with the path flags: the same pid, and its exit code.
#[test]
fn daemon_run_execs_reef_daemon() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    let exe = lone_reef(tmp.path());
    let script = exe.with_file_name("reef-daemon");
    std::fs::write(
        &script,
        "#!/bin/sh\necho \"pid $$\"\necho \"args $*\"\necho \"state $REEF_STATE_DIR\"\nexit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let child = lone_command(&exe, &state)
        .args(["--state-dir", state.to_str().unwrap(), "daemon", "run"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let pid = child.id();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(7), "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        format!(
            "pid {pid}\nargs --foreground --state-dir {}\nstate {}\n",
            state.display(),
            state.display()
        )
    );
}

/// R2.05 test 4: a `reef` with no `reef-daemon` beside it or on `PATH`
/// cannot start a daemon, run one, or show its configuration: exit 3,
/// naming the fix, and nothing created.
#[test]
fn no_daemon_binary() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    let exe = lone_reef(tmp.path());
    for args in [
        &["repo", "list"][..],
        &["daemon", "run"],
        &["config", "show"],
        &["restore", "/nonexistent"],
    ] {
        let output = lone_command(&exe, &state)
            .arg("-o")
            .arg("json")
            .args(args)
            .output()
            .unwrap();
        let text = failed(&output, 3, "not_found");
        assert!(text.contains("no reef-daemon beside"), "{args:?}: {text}");
        let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
        let hint = value["error"]["hint"].as_str().unwrap();
        assert!(
            hint.contains("reef-daemon")
                && hint.contains("cargo install")
                && hint.contains("reef-vcs reef-daemon"),
            "{hint}"
        );
        assert!(!state.exists(), "{args:?} created the state directory");
    }
}

/// R2.05 test 6: a `reef-daemon` that fails without its error object is
/// an internal error naming it.
#[test]
fn helper_without_an_error_object() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    let exe = lone_reef(tmp.path());
    let script = exe.with_file_name("reef-daemon");
    std::fs::write(&script, "#!/bin/sh\nexit 1\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let output = lone_command(&exe, &state)
        .args(["-o", "json", "config", "show"])
        .output()
        .unwrap();
    let text = failed(&output, 1, "internal");
    assert!(text.contains(&script.display().to_string()), "{text}");
}

/// R2.05 test 9: the CLI reads three keys of `config.toml` and ignores the
/// rest, which only the daemon validates.
#[tokio::test(flavor = "multi_thread")]
async fn client_ignores_keys_it_does_not_read() {
    let daemon = TestDaemon::start().await;
    std::fs::write(
        daemon.state_dir().join("config.toml"),
        "[watcher]\nno_such_key = 1\n",
    )
    .unwrap();
    let output = run(reef(&daemon), &["ws", "list"]).await;
    assert_eq!(json(&output), Value::Array(vec![]));
    // The daemon's own view still refuses it.
    let output = run(reef(&daemon), &["-o", "json", "config", "show"]).await;
    let text = failed(&output, 2, "invalid_argument");
    assert!(text.contains("unknown key `watcher.no_such_key`"), "{text}");
}

/// The agent of the operation that created workspace `id`.
async fn create_agent(client: &reef_client::Client, repo: &str, id: &str) -> String {
    use reef_api::v1::{ListOperationsRequest, OperationFilter, OperationKind};
    let page = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.to_owned(),
            filter: Some(OperationFilter {
                workspaces: vec![id.to_owned()],
                kinds: vec![OperationKind::CreateWorkspace.into()],
                ..OperationFilter::default()
            }),
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(page.operations.len(), 1, "{page:?}");
    page.operations[0].agent.clone()
}

/// R2.05 test 10: `--agent`, else `REEF_AGENT`, names every operation the
/// CLI causes; with neither the daemon records the owner. A bad name is
/// refused before any call.
#[tokio::test(flavor = "multi_thread")]
async fn agent_names_operations() {
    let (daemon, repo) = with_repo().await;
    let client = reef_client::Client::connect(&daemon.state_dir().join("daemon.sock"))
        .await
        .unwrap();
    let create = |agent_flag: Option<&'static str>, agent_env: Option<&'static str>| {
        let mut command = reef(&daemon);
        if let Some(agent) = agent_env {
            command.env("REEF_AGENT", agent);
        }
        if let Some(agent) = agent_flag {
            command.args(["--agent", agent]);
        }
        command.args(["ws", "create", "--repo", &repo.id]);
        run(command, &[])
    };
    for (flag, env, expected) in [
        (Some("bot-1"), None, "bot-1"),
        (None, Some("bot-2"), "bot-2"),
        (Some("bot-3"), Some("bot-2"), "bot-3"),
    ] {
        let ws = json(&create(flag, env).await);
        let id = ws["id"].as_str().unwrap();
        assert_eq!(create_agent(&client, &repo.id, id).await, expected);
    }
    let ws = json(&create(None, None).await);
    assert_eq!(
        create_agent(&client, &repo.id, ws["id"].as_str().unwrap()).await,
        ws["owner"].as_str().unwrap()
    );

    let long = "x".repeat(257);
    for bad in ["bot\u{1}", long.as_str()] {
        let output = run(reef(&daemon), &["--agent", bad, "ws", "list"]).await;
        failed(&output, 2, "invalid_argument");
        let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
        assert_eq!(value["error"]["reason"], "INVALID_AGENT", "{value}");
        assert!(
            value["error"]["hint"]
                .as_str()
                .unwrap()
                .contains("REEF_AGENT"),
            "{value}"
        );
    }
    // Refused before the socket is tried: no daemon, and still exit 2.
    let tmp = short_tempdir();
    let output = reef_in(&tmp.path().join("state"))
        .args(["--no-autostart", "--agent", "bot\u{1}", "ws", "list"])
        .output()
        .unwrap();
    failed(&output, 2, "invalid_argument");
}

/// R2.05 test 12: a path in no registered repository, and a repository or
/// workspace that does not exist, carry a reason.
#[tokio::test(flavor = "multi_thread")]
async fn not_found_reasons() {
    let (daemon, _repo) = with_repo().await;
    let reason = |output: &Output| -> Value {
        let value: Value = serde_json::from_str(&stdout(output)).unwrap();
        value["error"]["reason"].clone()
    };
    let plain = plain_git_repo(&daemon);
    let output = run(
        reef(&daemon),
        &[
            "--no-register",
            "ws",
            "create",
            "--repo",
            plain.to_str().unwrap(),
        ],
    )
    .await;
    failed(&output, 3, "not_found");
    assert_eq!(reason(&output), "REPOSITORY_NOT_REGISTERED");
    assert_eq!(next_of(&output)[..3], ["reef", "repo", "register"]);

    let output = run(reef(&daemon), &["ws", "inspect", "abcdabcdabcdabcd"]).await;
    failed(&output, 3, "not_found");
    assert_eq!(reason(&output), "WORKSPACE_NOT_FOUND");

    let output = run(reef(&daemon), &["ws", "list", "--repo", "abcd"]).await;
    failed(&output, 3, "not_found");
    assert_eq!(reason(&output), "REPOSITORY_NOT_FOUND");
}

#[tokio::test(flavor = "multi_thread")]
async fn repo_forget() {
    let (daemon, repo) = with_repo().await;
    let created = create(&daemon, &repo).await;
    // Refused while a workspace is active.
    failed(
        &run(reef(&daemon), &["repo", "forget", &repo.id]).await,
        5,
        "conflict",
    );
    json(
        &run(
            reef(&daemon),
            &["ws", "release", created["id"].as_str().unwrap()],
        )
        .await,
    );
    // By path, in text mode: a note naming the full id.
    let output = run(
        reef(&daemon),
        &["-o", "text", "repo", "forget", repo.root.to_str().unwrap()],
    )
    .await;
    output.clone().assert().success();
    assert_eq!(stdout(&output), "");
    assert_eq!(stderr(&output), format!("forgot {}\n", repo.id));
    assert_eq!(
        json(&run(reef(&daemon), &["repo", "list"]).await),
        Value::Array(vec![])
    );
    failed(
        &run(reef(&daemon), &["repo", "forget", &repo.id]).await,
        3,
        "not_found",
    );
}

/// `reef restore` (R1.18) is offline: a backup that is not there fails
/// with `not_found` and no daemon is started.
#[test]
fn restore_never_starts_a_daemon() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    let missing = tmp.path().join("no-backup");
    let output = reef_in(&state)
        .args(["restore", missing.to_str().unwrap()])
        .output()
        .unwrap();
    let text = failed(&output, 3, "not_found");
    assert!(text.contains("manifest.json"), "{text}");
    assert!(!state.exists(), "no daemon was started");
}

/// `reef backup` (R1.18) writes a backup and prints its manifest; a second
/// into the same directory is `already_exists`; `reef restore` refuses
/// while the daemon runs and names the command that stops it.
#[tokio::test(flavor = "multi_thread")]
async fn backup_then_restore_refuses_a_running_daemon() {
    let (daemon, repo) = with_repo().await;
    let dest = daemon.tmp.path().join("backup");
    let dest_text = dest.to_str().unwrap();
    let manifest = json(&run(reef(&daemon), &["backup", dest_text]).await);
    assert_eq!(manifest["complete"], true, "{manifest}");
    assert_eq!(manifest["repos"][0]["id"], repo.id.as_str(), "{manifest}");
    assert!(dest.join("manifest.json").is_file());
    assert!(dest.join("reef.sqlite3").is_file());

    let again = run(reef(&daemon), &["backup", dest_text]).await;
    failed(&again, 4, "already_exists");

    let restore = run(reef(&daemon), &["restore", dest_text, "--force"]).await;
    failed(&restore, 6, "unavailable");
    let value: Value = serde_json::from_str(&stdout(&restore)).unwrap();
    assert_eq!(
        value["error"]["next"],
        serde_json::json!(["reef", "daemon", "stop"]),
        "{value}"
    );
}

/// `ws gc --dry-run` (R1.17, moved here from R1.22 with the subcommand):
/// on a fresh daemon it reports every category with zero bytes and exits
/// 0; with a repository, one line for it.
#[tokio::test(flavor = "multi_thread")]
async fn ws_gc_dry_run() {
    let daemon = TestDaemon::start().await;
    let value = json(&run(reef(&daemon), &["ws", "gc", "--dry-run"]).await);
    assert_eq!(value, serde_json::json!([]));
    let output = run(reef(&daemon), &["-o", "text", "ws", "gc", "--dry-run"]).await;
    output.clone().assert().success();
    assert_eq!(
        stdout(&output),
        "total (dry run): jj 0 B -> 0 B, bases 0 removed (0 B), workspaces 0 rows (0 B), stores 0\n"
    );
    daemon.stop().await;

    let (daemon, repo) = with_repo().await;
    let value = json(
        &run(
            reef(&daemon),
            &["ws", "gc", "--dry-run", "--repo", &repo.id],
        )
        .await,
    );
    let reports = value.as_array().unwrap();
    assert_eq!(reports.len(), 1);
    assert_eq!(reports[0]["repo_id"], repo.id.as_str());
    assert_eq!(reports[0]["dry_run"], true);
    assert_eq!(reports[0]["jj_error"], Value::Null);
    assert!(reports[0]["jj_bytes_before"].as_u64().unwrap() > 0);
    let output = run(reef(&daemon), &["-o", "text", "ws", "gc", "--dry-run"]).await;
    output.clone().assert().success();
    let text = stdout(&output);
    assert!(
        text.starts_with(&format!("repo {} (dry run): jj ", repo.id)),
        "{text}"
    );
    assert_eq!(text.lines().count(), 2, "{text}");
}

/// Writes `files` at the registered root and snapshots its `default`
/// workspace, which a build without `--rev` builds from.
async fn commit_at_root(root: &Path, files: &[(&str, &str)]) {
    for (path, contents) in files {
        std::fs::write(root.join(path), contents).unwrap();
    }
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let settings = reef_daemon::testing::test_settings();
            let mut repo = reef_lib::jj::Repo::open(&root, &settings).await.unwrap();
            repo.snapshot("edit reef.toml").await.unwrap();
        });
    })
    .await
    .unwrap();
}

/// A `reef.toml` with store `deps` built by `sh -c <script>`.
fn store_spec(script: &str) -> String {
    format!(
        "[[store]]\nname = \"deps\"\nlockfile = \"deps.lock\"\nmount = \"vendor/deps\"\nbuild = [\"sh\", \"-c\", {script:?}]\n"
    )
}

/// RX.01 test 10 (S-03): `store build` shows the command the daemon runs,
/// from the repository's `reef.toml`, on stderr before its output, in
/// either mode.
#[tokio::test(flavor = "multi_thread")]
async fn store_build_prints_the_command_first() {
    let (daemon, repo) = with_repo().await;
    let spec = format!(
        "{}[store.env]\nNODE_ENV = \"production\"\n",
        store_spec("echo first >&2; mkdir -p \"$REEF_STORE_DIR\"")
    );
    commit_at_root(&repo.root, &[("reef.toml", &spec), ("deps.lock", "v1\n")]).await;
    for mode in ["text", "json"] {
        let output = run(
            reef(&daemon),
            &[
                "-o", mode, "store", "build", "deps", "--force", "--repo", &repo.id,
            ],
        )
        .await;
        output.clone().assert().success();
        let text = stderr(&output);
        let mut lines = text.lines();
        let first = lines.next().unwrap_or_default();
        assert!(
            first.starts_with("reef: building deps at ")
                && first.ends_with(": sh -c 'echo first >&2; mkdir -p \"$REEF_STORE_DIR\"'"),
            "{mode}: {text}"
        );
        assert_eq!(
            lines.next(),
            Some("reef: env: NODE_ENV=production"),
            "{mode}: {text}"
        );
        assert_eq!(lines.next(), Some("first"), "{mode}: {text}");
    }
}

/// `store build` and `store list` (R1.20) in JSON and text: the build's
/// output streams through (all on stderr in JSON mode), JSON mode prints
/// one result, and a failed build exits with its command's code.
#[tokio::test(flavor = "multi_thread")]
async fn store_build_and_list() {
    let (daemon, repo) = with_repo().await;
    let spec = store_spec("echo to-stdout; echo to-stderr >&2; mkdir -p \"$REEF_STORE_DIR\"");
    commit_at_root(&repo.root, &[("reef.toml", &spec), ("deps.lock", "v1\n")]).await;

    let output = run(
        reef(&daemon),
        &["store", "build", "deps", "--repo", &repo.id],
    )
    .await;
    let value = json(&output);
    assert_eq!(value["name"], "deps");
    assert_eq!(value["sealed"], true);
    assert_eq!(value["exit_code"], 0);
    assert_eq!(value["already_built"], false);
    let text = stderr(&output);
    assert!(
        text.contains("to-stdout") && text.contains("to-stderr"),
        "{text}"
    );

    let output = run(
        reef(&daemon),
        &["-o", "text", "store", "build", "deps", "--repo", &repo.id],
    )
    .await;
    output.clone().assert().success();
    assert!(
        stderr(&output).contains("is already built at"),
        "{}",
        stderr(&output)
    );
    assert_eq!(stdout(&output), "");

    let output = run(
        reef(&daemon),
        &[
            "-o", "text", "store", "build", "deps", "--force", "--repo", &repo.id,
        ],
    )
    .await;
    output.clone().assert().success();
    assert_eq!(stdout(&output), "to-stdout\n");
    let text = stderr(&output);
    assert!(
        text.contains("to-stderr") && text.contains("built store deps"),
        "{text}"
    );

    let listed = json(&run(reef(&daemon), &["store", "list"]).await);
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["name"], "deps");
    assert_eq!(listed[0]["in_use_by"], 0);
    assert_eq!(listed[0]["sealed"], true);
    let output = run(reef(&daemon), &["-o", "text", "store", "list"]).await;
    output.clone().assert().success();
    let text = stdout(&output);
    let mut lines = text.lines();
    assert!(lines.next().unwrap().starts_with("NAME"), "{text}");
    assert!(lines.next().unwrap().starts_with("deps "), "{text}");

    // The workspace shows its store.
    let ws = json(&run(reef(&daemon), &["ws", "create", "--repo", &repo.id]).await);
    assert_eq!(ws["stores"][0]["name"], "deps");
    assert_eq!(ws["stores"][0]["mount"], "vendor/deps");

    let failing = store_spec("exit 7");
    commit_at_root(
        &repo.root,
        &[("reef.toml", &failing), ("deps.lock", "v2\n")],
    )
    .await;
    let output = run(
        reef(&daemon),
        &["store", "build", "deps", "--repo", &repo.id],
    )
    .await;
    output.clone().assert().code(7);
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["sealed"], false);
    assert_eq!(value["exit_code"], 7);
    // Without the store, a create fails unless told to skip it.
    let output = run(reef(&daemon), &["ws", "create", "--repo", &repo.id]).await;
    let text = failed(&output, 6, "unavailable");
    assert!(
        text.contains("\nnext: reef store build deps --repo "),
        "{text}"
    );
    let ws = json(
        &run(
            reef(&daemon),
            &["ws", "create", "--skip-missing-stores", "--repo", &repo.id],
        )
        .await,
    );
    assert_eq!(ws["attention"], "store-missing");
}

/// `reef` against `daemon`, run from `dir`.
fn reef_at(daemon: &TestDaemon, dir: &Path) -> Command {
    let mut command = reef(daemon);
    command.current_dir(dir);
    command
}

/// A directory under the daemon's temporary directory, created.
fn new_dir(daemon: &TestDaemon, name: &str) -> PathBuf {
    let dir = daemon.tmp.path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// A plain git repository with one commit on `main`, no jj.
fn plain_git_repo(daemon: &TestDaemon) -> PathBuf {
    let root = new_dir(daemon, "plain-git");
    reef_testing::git::init_repo(&root, &[("README", "plain git\n")]);
    root
}

/// R1.25: `ws run` in a git checkout nobody registered initializes jj
/// alongside git, registers it, says so on stderr, and runs.
#[tokio::test(flavor = "multi_thread")]
async fn ws_run_registers_a_plain_git_repo() {
    let daemon = TestDaemon::start().await;
    let root = plain_git_repo(&daemon);

    let output = run(reef_at(&daemon, &root), &["ws", "run", "--", "true"]).await;
    output.clone().assert().success();
    let text = stderr(&output);
    assert!(
        text.contains(&format!(
            "reef: registered {} and initialized jj alongside git",
            root.display()
        )),
        "{text}"
    );
    assert!(root.join(".jj").is_dir());
    let exclude = std::fs::read_to_string(root.join(".git/info/exclude")).unwrap();
    assert!(exclude.lines().any(|line| line == "/.jj/"), "{exclude}");

    let listed = json(&run(reef(&daemon), &["repo", "list"]).await);
    let listed = listed.as_array().unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0]["root"], root.to_str().unwrap());
    assert_eq!(listed[0]["colocated"], true);
}

/// R1.31: a git checkout as `git clone` leaves it, from a remote whose
/// default branch is `develop`: `develop` checked out, `origin/HEAD`
/// pointing at it, and `main` only on the remote. Made with gix, as a clone
/// lays the refs out.
fn cloned_with_default_develop(daemon: &TestDaemon) -> PathBuf {
    use reef_testing::git::{commit, init_repo};
    let root = new_dir(daemon, "clone");
    let main = init_repo(&root, &[("README", "main\n")]);
    let develop = commit(
        &root,
        "refs/remotes/origin/develop",
        Some(&main),
        &[("README", "main\n"), ("DEVELOP", "develop\n")],
    );
    let git = root.join(".git");
    std::fs::rename(
        git.join("refs/heads/main"),
        git.join("refs/remotes/origin/main"),
    )
    .unwrap();
    std::fs::write(git.join("refs/heads/develop"), format!("{develop}\n")).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/develop\n").unwrap();
    std::fs::write(
        git.join("refs/remotes/origin/HEAD"),
        "ref: refs/remotes/origin/develop\n",
    )
    .unwrap();
    root
}

/// R1.31 test 2: with no local `main`, a create without a start follows
/// `origin/HEAD` to `develop` and says which bookmark it chose.
#[tokio::test(flavor = "multi_thread")]
async fn default_start_follows_origin_head() {
    let daemon = TestDaemon::start().await;
    let root = cloned_with_default_develop(&daemon);

    let output = run(reef_at(&daemon, &root), &["ws", "create", "-o", "json"]).await;
    let created = json(&output);
    assert_eq!(created["start"], "develop", "{created}");
    let path = Path::new(created["path"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(path.join("DEVELOP")).unwrap(),
        "develop\n"
    );
    assert!(
        !stderr(&output).contains("root checkout"),
        "{}",
        stderr(&output)
    );

    // A named start reports none.
    let output = run(
        reef_at(&daemon, &root),
        &["ws", "create", "--new-on", "develop", "-o", "json"],
    )
    .await;
    assert_eq!(json(&output)["start"], Value::Null);
}

/// R1.31 test 3: with none of the bookmarks, `ws create` and `ws run` start
/// on the root checkout's change, report it, and say so on stderr.
#[tokio::test(flavor = "multi_thread")]
async fn default_start_falls_back_and_says_so() {
    let daemon = TestDaemon::start().await;
    let root = new_dir(&daemon, "feature-only");
    reef_testing::git::init_repo(&root, &[("README", "feature\n")]);
    let git = root.join(".git");
    std::fs::rename(git.join("refs/heads/main"), git.join("refs/heads/feature")).unwrap();
    std::fs::write(git.join("HEAD"), "ref: refs/heads/feature\n").unwrap();
    let line = "reef: no main, master or trunk bookmark; starting on the root checkout's change (pass --new-on <branch>)";

    let output = run(reef_at(&daemon, &root), &["ws", "create", "-o", "json"]).await;
    let created = json(&output);
    assert_eq!(created["start"], "root checkout", "{created}");
    assert!(stderr(&output).contains(line), "{}", stderr(&output));
    let path = Path::new(created["path"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(path.join("README")).unwrap(),
        "feature\n"
    );

    let output = run(
        reef_at(&daemon, &root),
        &["ws", "run", "-o", "json", "--", "true"],
    )
    .await;
    let report = json(&output);
    assert_eq!(report["start"], "root checkout", "{report}");
    assert!(stderr(&output).contains(line), "{}", stderr(&output));

    // `--new-on` names the start: no line.
    let output = run(
        reef_at(&daemon, &root),
        &["ws", "create", "--new-on", "feature", "-o", "json"],
    )
    .await;
    assert_eq!(json(&output)["start"], Value::Null);
    assert!(!stderr(&output).contains(line), "{}", stderr(&output));
}

/// R1.25: an existing jj repository is registered without initializing
/// anything, and only the first command says so.
#[tokio::test(flavor = "multi_thread")]
async fn ws_create_registers_an_existing_jj_repo() {
    let daemon = TestDaemon::start().await;
    let root = new_dir(&daemon, "jj-repo");
    let at = root.clone();
    tokio::task::spawn_blocking(move || init_synthetic_repo(&at, Preset::Small))
        .await
        .unwrap();

    let output = run(reef_at(&daemon, &root), &["ws", "create", "-o", "json"]).await;
    let created = json(&output);
    assert!(created["path"].is_string(), "{created}");
    let text = stderr(&output);
    assert!(
        text.contains(&format!("reef: registered {}\n", root.display())),
        "{text}"
    );
    assert!(!text.contains("initialized"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn implicit_registration_is_once() {
    let daemon = TestDaemon::start().await;
    let root = plain_git_repo(&daemon);
    let first = run(reef_at(&daemon, &root), &["ws", "create"]).await;
    json(&first);
    assert!(stderr(&first).contains("registered"), "{}", stderr(&first));

    let second = run(reef_at(&daemon, &root), &["ws", "create"]).await;
    json(&second);
    assert!(
        !stderr(&second).contains("registered"),
        "{}",
        stderr(&second)
    );
    let listed = json(&run(reef(&daemon), &["repo", "list"]).await);
    assert_eq!(listed.as_array().unwrap().len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn no_register_flag_keeps_not_found() {
    let daemon = TestDaemon::start().await;
    let root = plain_git_repo(&daemon);

    let output = run(
        reef_at(&daemon, &root),
        &["--no-register", "-o", "json", "ws", "run", "--", "true"],
    )
    .await;
    let text = failed(&output, 3, "not_found");
    assert!(text.contains("repository containing"), "{text}");
    assert!(!root.join(".jj").exists());

    std::fs::write(
        daemon.state_dir().join("config.toml"),
        "[repo]\nauto_register = false\n",
    )
    .unwrap();
    let output = run(reef_at(&daemon, &root), &["ws", "create"]).await;
    let text = failed(&output, 3, "not_found");
    assert!(text.contains("repository containing"), "{text}");
    assert!(!root.join(".jj").exists());
    let listed = json(&run(reef(&daemon), &["repo", "list"]).await);
    assert_eq!(listed.as_array().unwrap().len(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn git_worktree_is_refused() {
    let daemon = TestDaemon::start().await;
    let worktree = new_dir(&daemon, "worktree");
    std::fs::write(
        worktree.join(".git"),
        "gitdir: /elsewhere/.git/worktrees/w\n",
    )
    .unwrap();

    let output = run(reef_at(&daemon, &worktree), &["ws", "create"]).await;
    let text = failed(&output, 2, "invalid_argument");
    assert!(text.contains("worktree or submodule"), "{text}");
    assert!(!worktree.join(".jj").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_dir_is_not_found() {
    let daemon = TestDaemon::start().await;
    let empty = new_dir(&daemon, "empty");

    let output = run(
        reef_at(&daemon, &empty),
        &["-o", "json", "ws", "run", "--", "true"],
    )
    .await;
    let text = failed(&output, 3, "not_found");
    assert!(text.contains("run git init first"), "{text}");
    assert!(!empty.join(".jj").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn repo_register_no_init() {
    let daemon = TestDaemon::start().await;
    let root = plain_git_repo(&daemon);
    let path = root.to_str().unwrap();

    let output = run(reef(&daemon), &["repo", "register", "--no-init", path]).await;
    failed(&output, 3, "not_found");
    assert!(!root.join(".jj").exists());

    let output = run(reef(&daemon), &["repo", "register", path]).await;
    let registered = json(&output);
    assert_eq!(registered["root"], path);
    assert_eq!(registered["colocated"], true);
    assert!(
        stderr(&output).contains("initialized jj alongside git"),
        "{}",
        stderr(&output)
    );
}

// R1.26: the agent-facing contract (ADR 0025).

/// Every runnable command as its words, from the guide's JSON form.
fn leaf_commands(state: &Path) -> Vec<Vec<String>> {
    let value = json(&reef_in(state).args(["-o", "json"]).output().unwrap());
    assert!(value["guide"].as_str().unwrap().starts_with("# Reef"));
    serde_json::from_value(value["commands"].clone()).unwrap()
}

/// The subcommands `--help` lists for `path`, without clap's `help`.
fn help_commands(state: &Path, path: &[String]) -> Vec<String> {
    let output = reef_in(state).args(path).arg("--help").output().unwrap();
    output.clone().assert().success();
    stdout(&output)
        .lines()
        .skip_while(|line| *line != "Commands:")
        .skip(1)
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_whitespace().next())
        .filter(|name| *name != "help")
        .map(str::to_owned)
        .collect()
}

/// Whether `next` is `reef`, then global flags with their values, then a
/// command's words and its arguments.
fn is_real_command(next: &[String], commands: &[Vec<String>]) -> bool {
    let Some((first, mut rest)) = next.split_first() else {
        return false;
    };
    if first != "reef" {
        return false;
    }
    while let [flag, _value, tail @ ..] = rest {
        if !flag.starts_with("--") {
            break;
        }
        rest = tail;
    }
    commands
        .iter()
        .any(|command| rest.len() >= command.len() && rest[..command.len()] == command[..])
}

/// `error.next` of a failed command's JSON.
fn next_of(output: &Output) -> Vec<String> {
    let value: Value = serde_json::from_str(&stdout(output)).unwrap();
    serde_json::from_value(value["error"]["next"].clone())
        .unwrap_or_else(|e| panic!("{e}: {value}"))
}

fn words(argv: &[String]) -> Vec<&str> {
    argv.iter().map(String::as_str).collect()
}

#[test]
fn no_args_prints_guide_and_exits_zero() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    let output = reef_in(&state).output().unwrap();
    output.clone().assert().success();
    let text = stdout(&output);
    assert!(text.starts_with("# Reef\n"), "{text}");
    assert!(text.len() <= 1600, "{} bytes", text.len());
    assert!(!text.contains("reef:long"), "{text}");
    assert!(text.contains("reef ws run -- <command>"), "{text}");
    for args in [&["guide"][..], &["agent-guide"], &["gui"]] {
        let output = reef_in(&state).args(args).output().unwrap();
        assert_eq!(stdout(&output), text, "{args:?}");
    }
    let short = stdout(&reef_in(&state).args(["guide", "--short"]).output().unwrap());
    assert!(
        text.starts_with(&short) && short.len() < text.len(),
        "{short}"
    );
    assert!(short.lines().count() <= 40);
    assert!(!state.exists(), "no daemon was started");
}

#[test]
fn guide_json_lists_every_leaf_command() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    let mut listed = leaf_commands(&state);
    // The same tree, walked through `--help`.
    let mut walked = Vec::new();
    let mut pending = vec![Vec::<String>::new()];
    while let Some(path) = pending.pop() {
        let children = help_commands(&state, &path);
        for child in &children {
            let mut child_path = path.clone();
            child_path.push(child.clone());
            pending.push(child_path);
        }
        if children.is_empty() {
            walked.push(path);
        }
    }
    listed.sort();
    walked.sort();
    assert_eq!(listed, walked);
    assert!(listed.contains(&vec!["ws".to_owned(), "run".to_owned()]));
    assert!(listed.contains(&vec!["here".to_owned()]));
}

#[test]
fn help_fits_sixty_lines() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    let mut paths = vec![Vec::new()];
    paths.extend(
        help_commands(&state, &[])
            .into_iter()
            .map(|noun| vec![noun]),
    );
    for command in leaf_commands(&state) {
        paths.push(command);
    }
    for path in paths {
        let output = reef_in(&state).args(&path).arg("--help").output().unwrap();
        let lines = stdout(&output).lines().count();
        assert!(lines <= 60, "reef {} --help: {lines} lines", path.join(" "));
    }
}

/// Every command finishes with stdin closed: none prompts, pages, or
/// waits on input. Run against a live daemon from a registered checkout,
/// so each command gets past connecting; `ws run -- cat` is the one whose
/// child reads stdin. `daemon run`, `start`, and `stop` are left out: they
/// start a daemon or signal this process, which hosts the test daemon.
#[tokio::test(flavor = "multi_thread")]
async fn every_command_completes_with_stdin_closed() {
    let (daemon, repo) = with_repo().await;
    let state = daemon.state_dir().to_owned();
    let mut lines: Vec<Vec<String>> = Vec::new();
    for mut command in leaf_commands(&state) {
        // `op watch` and `presence watch` wait 30 s by default.
        if command == ["op", "watch"] || command == ["presence", "watch"] {
            command.extend(["--for".to_owned(), "1".to_owned()]);
        }
        // `ui` never opens a browser here (stdout is not a terminal), and
        // `--print` asks for no launch file either.
        if command == ["ui"] {
            command.push("--print".to_owned());
        }
        if command.first().is_some_and(|noun| noun == "daemon")
            && command.get(1).is_some_and(|verb| verb != "status")
        {
            continue;
        }
        // `ws run bogus` would make a workspace to fail in; `ws run -- cat`
        // below covers the command that runs.
        if command != ["ws", "run"] {
            let mut bogus = command.clone();
            bogus.push("bogus".to_owned());
            lines.push(bogus);
        }
        lines.push(command);
    }
    for line in [
        &["ws", "run", "--", "cat"][..],
        &["repo", "register", "."],
        &[],
    ] {
        lines.push(line.iter().map(|word| (*word).to_owned()).collect());
    }
    let root = repo.root.clone();
    tokio::task::spawn_blocking(move || {
        for line in lines {
            let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_reef"));
            for var in [
                "REEF_SOCKET",
                "REEF_CONFIG",
                "REEF_OUTPUT",
                "REEF_WORKSPACE",
                "REEF_AGENT",
            ] {
                command.env_remove(var);
            }
            command
                .env("REEF_STATE_DIR", &state)
                .current_dir(&root)
                .arg("--no-autostart")
                .args(&line)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut child = command.spawn().unwrap();
            let started = Instant::now();
            // Generous: a real `ws gc` or `ws create` under a loaded test
            // run takes seconds. A command waiting on stdin never finishes.
            while child.try_wait().unwrap().is_none() {
                if started.elapsed() > Duration::from_secs(60) {
                    let _ = child.kill();
                    panic!("reef {} did not finish with stdin closed", line.join(" "));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            eprintln!("TIMING {:?} {}", started.elapsed(), line.join(" "));
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_store_error_carries_next() {
    let (daemon, repo) = with_repo().await;
    let spec = store_spec("mkdir -p \"$REEF_STORE_DIR\"");
    commit_at_root(&repo.root, &[("reef.toml", &spec), ("deps.lock", "v1\n")]).await;

    let output = run(
        reef(&daemon),
        &["ws", "create", "-o", "json", "--repo", &repo.id],
    )
    .await;
    failed(&output, 6, "unavailable");
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    let message = value["error"]["message"].as_str().unwrap();
    assert!(
        !message.contains("reef "),
        "the fix is not in the message: {message}"
    );
    assert!(value["error"]["hint"].is_string(), "{value}");
    let next = next_of(&output);
    assert_eq!(next[..4], ["reef", "store", "build", "deps"]);
    assert_eq!(next[4..6], ["--repo", repo.id.as_str()]);
    assert_eq!(next[6], "--rev");
    assert_eq!(next.len(), 8, "{next:?}");
    assert!(is_real_command(&next, &leaf_commands(daemon.state_dir())));

    let output = run(
        reef(&daemon),
        &["-o", "text", "ws", "create", "--repo", &repo.id],
    )
    .await;
    output.clone().assert().code(6);
    let text = stderr(&output);
    assert!(
        text.contains(&format!("\nnext: {}\n", next.join(" "))),
        "{text}"
    );

    // Run verbatim, it fixes what it was given for.
    json(&run(reef(&daemon), &words(&next[1..])).await);
    json(&run(reef(&daemon), &["ws", "create", "--repo", &repo.id]).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn every_next_is_a_real_command() {
    let (daemon, repo) = with_repo().await;
    let commands = leaf_commands(daemon.state_dir());
    let mut nexts = Vec::new();

    // A released workspace: create another, which works as given.
    let created = create(&daemon, &repo).await;
    let id = created["id"].as_str().unwrap();
    json(&run(reef(&daemon), &["ws", "release", id]).await);
    let output = run(reef(&daemon), &["ws", "release", id]).await;
    failed(&output, 5, "conflict");
    let next = next_of(&output);
    assert_eq!(next, ["reef", "ws", "create", "--repo", repo.id.as_str()]);
    json(&run(reef(&daemon), &words(&next[1..])).await);
    nexts.push(next);

    // A workspace nobody has: list them.
    let output = run(reef(&daemon), &["ws", "inspect", "abcdabcdabcdabcd"]).await;
    failed(&output, 3, "not_found");
    let next = next_of(&output);
    assert_eq!(next, ["reef", "ws", "list", "--all"]);
    nexts.push(next);

    // A checkout no registered repository contains: register it, which
    // works as given.
    let plain = plain_git_repo(&daemon);
    let output = run(reef_at(&daemon, &plain), &["--no-register", "ws", "create"]).await;
    failed(&output, 3, "not_found");
    let next = next_of(&output);
    assert_eq!(next, ["reef", "repo", "register", plain.to_str().unwrap()]);
    json(&run(reef(&daemon), &words(&next[1..])).await);
    nexts.push(next);

    // A path that does not exist: a hint, and the registered repositories.
    let output = run(
        reef(&daemon),
        &["ws", "create", "--repo", "/nonexistent/repo"],
    )
    .await;
    let text = failed(&output, 3, "not_found");
    assert!(
        text.contains("\nhint: /nonexistent/repo does not exist"),
        "{text}"
    );
    let next = next_of(&output);
    assert_eq!(next, ["reef", "repo", "list"]);
    nexts.push(next);

    // No daemon on the socket: start it.
    let output = run(
        reef(&daemon),
        &["--socket", "/nonexistent/reef.sock", "ws", "list"],
    )
    .await;
    failed(&output, 6, "unavailable");
    nexts.push(next_of(&output));

    for next in &nexts {
        assert!(is_real_command(next, &commands), "{next:?} in {commands:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn here_inside_ws_run() {
    let (daemon, repo) = with_repo().await;
    let created = create(&daemon, &repo).await;
    let id = created["id"].as_str().unwrap();
    let here = env!("CARGO_BIN_EXE_reef");

    let output = run(
        reef(&daemon),
        &[
            "ws",
            "run",
            "--workspace",
            id,
            "--keep",
            "--",
            here,
            "here",
            "-o",
            "json",
        ],
    )
    .await;
    let value = json(&output);
    assert_eq!(value["in_workspace"], true, "{value}");
    assert_eq!(value["id"], id, "{value}");
    assert_eq!(value["path"], created["path"], "{value}");
    assert_eq!(value["lease"]["state"], "active", "{value}");
    // `ws run` releases it, so there is nothing for the agent to run.
    assert_eq!(value["next"], serde_json::json!([]), "{value}");
    assert!(
        value["hint"].as_str().unwrap().contains("ws run"),
        "{value}"
    );

    let output = run(
        reef(&daemon),
        &[
            "ws",
            "run",
            "--workspace",
            id,
            "--keep",
            "--",
            here,
            "here",
            "-o",
            "text",
        ],
    )
    .await;
    output.clone().assert().success();
    let text = stdout(&output);
    assert!(text.lines().count() <= 12, "{text}");
    assert!(
        text.starts_with(&format!("workspace:     {id}\n")),
        "{text}"
    );
    assert!(text.contains("\nlease:         active, "), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn here_by_path() {
    let (daemon, repo) = with_repo().await;
    let created = create(&daemon, &repo).await;
    let id = created["id"].as_str().unwrap();
    let path = PathBuf::from(created["path"].as_str().unwrap());
    let inside = first_entry(&path, true);

    let value = json(&run(reef_at(&daemon, &inside), &["here"]).await);
    assert_eq!(value["in_workspace"], true, "{value}");
    assert_eq!(value["id"], id, "{value}");
    // No snapshot without --snapshot, and no disk walk.
    assert_eq!(
        value["last_snapshot_op"], created["last_snapshot_op"],
        "{value}"
    );
    assert!(value.get("disk_usage").is_none(), "{value}");
    assert_eq!(
        value["next"],
        serde_json::json!(["reef", "ws", "release", id])
    );

    std::fs::write(inside.join("new-file"), "edit\n").unwrap();
    let value = json(&run(reef_at(&daemon, &inside), &["here", "--snapshot"]).await);
    assert!(value["last_snapshot_op"].is_string(), "{value}");

    let output = run(reef_at(&daemon, &path), &["-o", "text", "here"]).await;
    output.clone().assert().success();
    let text = stdout(&output);
    assert!(text.lines().count() <= 12, "{text}");
    assert!(
        text.ends_with(&format!("next:          reef ws release {id}\n")),
        "{text}"
    );

    // Released: the directory is gone, and REEF_WORKSPACE still answers.
    json(&run(reef(&daemon), &["ws", "release", id]).await);
    let mut command = reef(&daemon);
    command.env("REEF_WORKSPACE", id);
    let value = json(&run(command, &["here"]).await);
    assert_eq!(value["state"], "released", "{value}");
    assert_eq!(
        value["next"],
        serde_json::json!(["reef", "ws", "create", "--repo", repo.id])
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn here_outside() {
    let (daemon, repo) = with_repo().await;
    let output = run(reef_at(&daemon, &repo.root), &["here"]).await;
    let value = json(&output);
    assert_eq!(value["in_workspace"], false, "{value}");
    assert_eq!(value["repo"]["id"], repo.id.as_str(), "{value}");
    assert_eq!(
        value["next"],
        serde_json::json!(["reef", "ws", "run", "--"])
    );

    let elsewhere = new_dir(&daemon, "elsewhere");
    let output = run(reef_at(&daemon, &elsewhere), &["-o", "text", "here"]).await;
    output.clone().assert().success();
    let text = stdout(&output);
    assert!(
        text.contains("\nrepo:      none registered here\n"),
        "{text}"
    );
    assert!(
        text.ends_with("next:      reef ws run -- <command>\n"),
        "{text}"
    );
    assert_eq!(stderr(&output), "");
}

#[tokio::test(flavor = "multi_thread")]
async fn aliases_and_prefixes_resolve() {
    let (daemon, repo) = with_repo().await;
    let created = create(&daemon, &repo).await;
    let id = created["id"].as_str().unwrap();
    for args in [
        &["workspace", "ls"][..],
        &["ws", "ls"],
        &["w", "l"],
        &["ws", "li"],
    ] {
        let listed = json(&run(reef(&daemon), args).await);
        assert_eq!(listed.as_array().unwrap().len(), 1, "{args:?}");
    }
    json(&run(reef(&daemon), &["repo", "ls"]).await);
    json(&run(reef(&daemon), &["store", "ls"]).await);
    for args in [
        &["ws", "cr", "--help"][..],
        &["ws", "new", "--help"],
        &["workspace", "create", "--help"],
        &["ws", "rm", "--help"],
        &["gu", "--short"],
    ] {
        run(reef(&daemon), args).await.assert().success();
    }
    // Ambiguous: release, rm, and run all start with r. clap refuses.
    run(reef(&daemon), &["ws", "r", "--help"])
        .await
        .assert()
        .code(2);
    json(&run(reef(&daemon), &["ws", "rm", id]).await);
}

#[test]
fn reef_output_env_sets_the_default() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    let with = |value: &str, args: &[&str]| {
        reef_in(&state)
            .env("REEF_OUTPUT", value)
            .arg("--no-autostart")
            .args(args)
            .output()
            .unwrap()
    };
    // Piped stdout defaults to JSON; REEF_OUTPUT=text overrides that.
    let output = with("text", &["daemon", "status"]);
    output.clone().assert().code(6);
    assert_eq!(stdout(&output), "");
    let output = with("json", &["daemon", "status"]);
    failed(&output, 6, "unavailable");
    // A usage error, read before clap has parsed anything.
    let output = with("text", &["ws"]);
    output.clone().assert().code(2);
    assert_eq!(stdout(&output), "");
    // -o beats REEF_OUTPUT.
    let output = with("text", &["-o", "json", "daemon", "status"]);
    failed(&output, 6, "unavailable");
    // Brief errors are the same object, compact; `-o json` beats it.
    let output = with("brief", &["daemon", "status"]);
    failed(&output, 6, "unavailable");
    assert_eq!(stdout(&output).lines().count(), 1, "{}", stdout(&output));
    let output = with("brief", &["-o", "json", "daemon", "status"]);
    failed(&output, 6, "unavailable");
    assert!(stdout(&output).lines().count() > 1, "{}", stdout(&output));
    // The guide is text unless JSON is asked for, by either.
    let value = json(&with("json", &["guide"]));
    assert!(value["guide"].is_string(), "{value}");
    let output = with("json", &["-o", "text", "guide"]);
    assert!(stdout(&output).starts_with("# Reef"));
}

#[tokio::test(flavor = "multi_thread")]
async fn ws_list_limit_and_truncated() {
    let (daemon, repo) = with_repo().await;
    let mut ids = Vec::new();
    for _ in 0..3 {
        ids.push(
            create(&daemon, &repo).await["id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    let output = run(reef(&daemon), &["ws", "list", "--limit", "2"]).await;
    let listed = json(&output);
    let listed: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|ws| ws["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, [ids[2].as_str(), ids[1].as_str()], "newest first");
    assert_eq!(
        stderr(&output),
        "reef: showing the newest 2 of 3 workspaces; --limit 0 shows all\n"
    );
    let all = json(&run(reef(&daemon), &["ws", "list", "--limit", "0"]).await);
    assert_eq!(all.as_array().unwrap().len(), 3);
    let quiet = run(reef(&daemon), &["-q", "ws", "list", "--limit", "1"]).await;
    assert_eq!(json(&quiet).as_array().unwrap().len(), 1);
    assert_eq!(stderr(&quiet), "");
    let output = run(reef(&daemon), &["ws", "list"]).await;
    assert_eq!(json(&output).as_array().unwrap().len(), 3);
    assert_eq!(stderr(&output), "");
}

/// Every `reef` invocation in `docs/quickstart.md` (R1.24), in its `sh`
/// blocks and in inline code elsewhere: its first two words name a command
/// `reef --help` knows, and each flag it passes is in that command's help.
/// Removing or renaming a command or flag the quickstart uses fails here;
/// the page must change with it.
#[test]
fn quickstart_commands_exist() {
    let page = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/quickstart.md");
    let text = std::fs::read_to_string(&page).unwrap();
    // Shell lines from `sh` blocks, and `reef ...` code spans outside blocks.
    let mut commands: Vec<String> = Vec::new();
    let mut fence: Option<&str> = None;
    for line in text.lines() {
        if let Some(info) = line.strip_prefix("```") {
            fence = if fence.is_some() { None } else { Some(info) };
            continue;
        }
        match fence {
            Some("sh") => commands.push(line.to_owned()),
            Some(_) => {}
            None => commands.extend(
                line.split('`')
                    .skip(1)
                    .step_by(2)
                    .filter(|span| *span == "reef" || span.starts_with("reef "))
                    .map(str::to_owned),
            ),
        }
    }
    let mut checked = 0;
    for line in &commands {
        let tokens: Vec<&str> = line.split_whitespace().collect();
        for (at, token) in tokens.iter().enumerate() {
            if *token != "reef" && !token.ends_with("(reef") {
                continue;
            }
            // Its arguments, up to the end of the command; `--remote NAME`
            // before the command chooses a daemon (R2.08), not a flag of it.
            let rest = match tokens.get(at + 1) {
                Some(&"--remote") => &tokens[(at + 3).min(tokens.len())..],
                _ => &tokens[at + 1..],
            };
            let args: Vec<&str> = rest
                .iter()
                .copied()
                .take_while(|arg| {
                    !matches!(*arg, "&&" | "||" | "|" | ";" | "--") && !arg.starts_with(')')
                })
                .collect();
            let words: Vec<&str> = args
                .iter()
                .copied()
                .take_while(|arg| !arg.starts_with('-') && !arg.starts_with('<'))
                .take(3)
                .collect();
            let mut help = reef_in(Path::new("/nonexistent"));
            help.args(&words).arg("--help");
            let output = help.output().unwrap();
            let shown = stdout(&output);
            assert!(
                output.status.success(),
                "`reef {}` in the quickstart is not a command:\n{line}\n{}",
                words.join(" "),
                stderr(&output)
            );
            for flag in args.iter().filter(|arg| arg.starts_with("--")) {
                let flag = flag.split('=').next().unwrap_or(flag);
                assert!(
                    shown.contains(flag),
                    "`reef {}` has no {flag}, which the quickstart passes:\n{line}",
                    words.join(" ")
                );
            }
            checked += 1;
        }
    }
    assert!(
        checked >= 12,
        "only {checked} reef invocations found in {}",
        page.display()
    );
}

/// R2.02: `--new-on` and `--change` take a jj revset naming one commit; a
/// divergent change names two, which is `invalid_argument` (exit 2) with
/// reason `AMBIGUOUS_REVISION`.
#[tokio::test(flavor = "multi_thread")]
async fn ws_create_takes_a_revset() {
    let (daemon, repo) = with_repo().await;
    let first = create(&daemon, &repo).await;
    let change = first["change_id"].as_str().unwrap();
    let get = async |revision: String| {
        daemon
            .client
            .get_change(reef_api::v1::GetChangeRequest {
                repo: repo.id.clone(),
                revision,
                ..reef_api::v1::GetChangeRequest::default()
            })
            .await
            .unwrap()
            .change
            .unwrap()
    };
    let root_checkout = get("default@".to_owned()).await.commit_id;

    // The parent of the first workspace's change.
    let parent = format!("{change}-");
    let value = json(
        &run(
            reef(&daemon),
            &[
                "ws", "create", "--repo", &repo.id, "--new-on", &parent, "-o", "json",
            ],
        )
        .await,
    );
    let below = get(format!("{}-", value["commit_id"].as_str().unwrap())).await;
    assert_eq!(below.commit_id, root_checkout);

    // Two workspaces on one change, both edited: the change is divergent.
    let second = json(
        &run(
            reef(&daemon),
            &[
                "ws", "create", "--repo", &repo.id, "--change", change, "-o", "json",
            ],
        )
        .await,
    );
    for (ws, file) in [(&first, "a.txt"), (&second, "b.txt")] {
        let path = Path::new(ws["path"].as_str().unwrap());
        std::fs::write(path.join(file), "edit\n").unwrap();
        daemon
            .client
            .snapshot_workspace("", ws["id"].as_str().unwrap(), "")
            .await
            .unwrap();
    }
    let output = run(
        reef(&daemon),
        &[
            "ws", "create", "--repo", &repo.id, "--change", change, "-o", "json",
        ],
    )
    .await;
    failed(&output, 2, "invalid_argument");
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["error"]["reason"], "AMBIGUOUS_REVISION", "{value}");
    assert!(
        value["error"]["hint"]
            .as_str()
            .unwrap()
            .contains("commit id"),
        "{value}"
    );
    daemon.stop().await;
}

/// The files at the top of `root`, by name, not hidden.
fn top_files(root: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| entry.file_name().into_string().unwrap())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names
}

/// The one listed workspace with `id`.
fn listed<'a>(list: &'a Value, id: &str) -> &'a Value {
    list.as_array()
        .unwrap()
        .iter()
        .find(|ws| ws["id"] == id)
        .unwrap_or_else(|| panic!("{id} not in {list}"))
}

// #114 test 13
#[tokio::test(flavor = "multi_thread")]
async fn ws_list_counts_changed_files() {
    let (daemon, repo) = with_repo().await;
    let busy = create(&daemon, &repo).await;
    let busy_id = busy["id"].as_str().unwrap();
    let fresh = create(&daemon, &repo).await;
    let fresh_id = fresh["id"].as_str().unwrap();
    let root = PathBuf::from(busy["path"].as_str().unwrap());
    let files = top_files(&root);
    std::fs::write(root.join(&files[0]), "modified\n").unwrap();
    std::fs::remove_file(root.join(&files[1])).unwrap();
    std::fs::write(root.join("added.txt"), "added\n").unwrap();
    json(&run(reef(&daemon), &["ws", "snapshot", busy_id]).await);

    let list = json(&run(reef(&daemon), &["ws", "list"]).await);
    assert_eq!(listed(&list, busy_id)["changed_file_count"], 3, "{list}");
    assert_eq!(listed(&list, fresh_id)["changed_file_count"], 0, "{list}");
    let full = json(&run(reef(&daemon), &["-o", "json", "ws", "list"]).await);
    assert_eq!(listed(&full, busy_id)["changed_file_count"], 3, "{full}");
    let output = run(reef(&daemon), &["-o", "text", "ws", "list"]).await;
    let text = stdout(&output);
    let row = text.lines().find(|line| line.starts_with(busy_id)).unwrap();
    assert_eq!(row.split_whitespace().nth(3), Some("3"), "{text}");

    json(&run(reef(&daemon), &["ws", "release", fresh_id]).await);
    let all = json(&run(reef(&daemon), &["ws", "list", "--all"]).await);
    assert!(
        listed(&all, fresh_id).get("changed_file_count").is_none(),
        "{all}"
    );
    assert_eq!(listed(&all, fresh_id)["state"], "released", "{all}");
    let all = json(&run(reef(&daemon), &["-o", "json", "ws", "list", "--all"]).await);
    assert_eq!(listed(&all, fresh_id)["changed_file_count"], Value::Null);
    let output = run(reef(&daemon), &["-o", "text", "ws", "list", "--all"]).await;
    let text = stdout(&output);
    let row = text
        .lines()
        .find(|line| line.starts_with(fresh_id))
        .unwrap();
    assert_eq!(row.split_whitespace().nth(3), Some("-"), "{text}");
}

// #114 test 14
#[tokio::test(flavor = "multi_thread")]
async fn ws_list_brief_names_the_repository_when_several() {
    let (daemon, repo) = with_repo().await;
    let root = daemon.tmp.path().join("second");
    let at = root.clone();
    tokio::task::spawn_blocking(move || init_synthetic_repo(&at, Preset::Small))
        .await
        .unwrap();
    let registered = json(&run(reef(&daemon), &["repo", "register", root.to_str().unwrap()]).await);
    let second = registered["id"].as_str().unwrap();
    create(&daemon, &repo).await;
    json(&run(reef(&daemon), &["ws", "create", "--repo", second]).await);

    let list = json(&run(reef(&daemon), &["ws", "list"]).await);
    let repos: Vec<&str> = list
        .as_array()
        .unwrap()
        .iter()
        .map(|ws| ws["repo_id"].as_str().unwrap())
        .collect();
    assert_eq!(repos.len(), 2, "{list}");
    assert!(
        repos.contains(&repo.id.as_str()) && repos.contains(&second),
        "{list}"
    );
    let one = json(&run(reef(&daemon), &["ws", "list", "--repo", second]).await);
    assert_eq!(one.as_array().unwrap().len(), 1, "{one}");
    assert!(one[0].get("repo_id").is_none(), "{one}");
}
