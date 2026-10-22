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

/// `reef` with a clean environment and the given state directory.
fn reef_in(state_dir: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    for var in [
        "REEF_SOCKET",
        "REEF_CONFIG",
        "REEF_LOG",
        "RUST_LOG",
        "REEF_LOG_FORMAT",
        "REEF_OUTPUT",
        "REEF_WORKSPACE",
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
        "ws",
        "here",
        "guide",
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
        (&["ws", "move", "abcd", "--change", "x"][..], "R1.06"),
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
/// waits on input. `daemon run` and `daemon start` start a daemon by design
/// and are left out; autostart's tests cover them.
#[test]
fn every_command_completes_with_stdin_closed() {
    let tmp = short_tempdir();
    let state = tmp.path().join("state");
    let _guard = StopOnDrop(state.clone());
    let mut lines: Vec<Vec<String>> = Vec::new();
    for command in leaf_commands(&state) {
        if command == ["daemon", "run"] || command == ["daemon", "start"] {
            continue;
        }
        let mut bogus = command.clone();
        bogus.push("bogus".to_owned());
        lines.push(command);
        lines.push(bogus);
    }
    lines.push(["ws", "run", "--", "true"].map(str::to_owned).to_vec());
    lines.push(Vec::new());
    for line in lines {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_reef"));
        for var in [
            "REEF_SOCKET",
            "REEF_CONFIG",
            "REEF_OUTPUT",
            "REEF_WORKSPACE",
        ] {
            command.env_remove(var);
        }
        command
            .env("REEF_STATE_DIR", &state)
            .arg("--no-autostart")
            .args(&line)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut child = command.spawn().unwrap();
        let started = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if started.elapsed() > Duration::from_secs(5) {
                let _ = child.kill();
                panic!("reef {} did not finish with stdin closed", line.join(" "));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert!(!state.join("daemon.pid").exists(), "no daemon was started");
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
