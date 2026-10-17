//! The `reef` CLI against a daemon (R1.22): output modes, exit codes,
//! autostart, and `daemon start|stop|status`. Tests 1 to 11 are the brief's;
//! R1.17 adds `ws_gc_dry_run`.
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
use std::process::Output;
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

/// `reef` with a clean environment and the given state directory.
fn reef_in(state_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    for var in [
        "REEF_SOCKET",
        "REEF_CONFIG",
        "REEF_LOG",
        "RUST_LOG",
        "REEF_LOG_FORMAT",
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

// 1
#[test]
fn help_lists_every_command() {
    let tmp = tempfile::tempdir().unwrap();
    let output = reef_in(tmp.path()).arg("--help").output().unwrap();
    let text = stdout(&output);
    for command in [
        "daemon",
        "config",
        "repo",
        "ws",
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
    for command in ["create", "list", "inspect", "release", "move", "run", "gc"] {
        assert!(
            text.lines()
                .any(|line| line.trim_start().starts_with(&format!("{command} "))),
            "{command}: {text}"
        );
    }
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn json_when_piped() {
    let (daemon, repo) = with_repo().await;
    let value = json(&run(reef(&daemon), &["repo", "list"]).await);
    let list = value.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], repo.id.as_str());

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

    let listed = json(&run(reef(&daemon), &["ws", "list"]).await);
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
    let output = run(reef(&daemon), &["-o", "text", "ws", "inspect", &id]).await;
    assert!(stdout(&output).starts_with("id:"), "{}", stdout(&output));

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
    let all = json(&run(reef(&daemon), &["ws", "list", "--all"]).await);
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
    // A copy of `reef` with no `reef-daemon` beside it or on PATH, so the
    // daemon started is this build's `reef daemon run`, never a stale
    // `reef-daemon` from another build.
    let bin = tmp.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    let exe = bin.join("reef");
    std::fs::copy(env!("CARGO_BIN_EXE_reef"), &exe).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _guard = StopOnDrop(state.clone());
    let reef = || {
        let mut command = Command::new(&exe);
        for var in ["REEF_SOCKET", "REEF_CONFIG", "REEF_LOG", "RUST_LOG"] {
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
    assert_eq!(
        kill(Pid::from_raw(i32::try_from(pid).unwrap()), None),
        Err(Errno::ESRCH)
    );
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

// 11
#[test]
fn no_jj_lib_in_binary() {
    fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                sources(&path, found);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    let mut files = Vec::new();
    sources(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut files,
    );
    assert!(files.len() > 10, "{files:?}");
    let needle = ["reef_lib", "::jj"].concat();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains(&needle), "{} uses {needle}", file.display());
    }
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

/// Stubs fail with their ticket and never start a daemon.
#[test]
fn stubs_exit_1_without_a_daemon() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    for (args, ticket) in [
        (&["ws", "gc"][..], "R1.17"),
        (&["ws", "move", "abcd", "--change", "x"], "R1.06"),
        (&["ws", "run", "--", "true"], "R1.21"),
        (&["store", "list"], "R1.20"),
        (&["backup", "/tmp/x"], "R1.18"),
        (&["restore", "/tmp/x"], "R1.18"),
    ] {
        let output = reef_in(&state).args(args).output().unwrap();
        let text = failed(&output, 1, "internal");
        assert!(
            text.contains(&format!("not implemented until {ticket}")),
            "{text}"
        );
    }
    assert!(!state.exists(), "no daemon was started");
}
