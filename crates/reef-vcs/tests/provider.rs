//! `reef ws provider` and `reef ws renew --follow` against a daemon
//! (R2.26). Numbered as the brief numbers them; tests 1 to 5 are unit
//! tests in `src/cli/provider.rs` and `src/cli/holder.rs`.
//!
//! Each test runs a [`TestDaemon`] in this process with a `small`
//! synthetic repository and short leases (`lease.default = 4s`, `grace =
//! 2s`, `heartbeat = 1s`, `tick = 500ms`), and the `reef` binary against
//! its socket with `--no-autostart`. Holders are `sleep` processes the
//! test starts and kills; every one is killed when the test ends, so no
//! follower outlives it.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon};
use reef_testing::synth::Preset;
use serde_json::{Value, json};

/// How long a follower may take to start and renew once, on a busy
/// machine running several daemons.
const RECORD_WITHIN: Duration = Duration::from_secs(15);
/// The lease of a held workspace: long enough that a follower slow to
/// start on a busy machine still renews it in time (a 4 s lease and 2 s
/// grace were not, with several daemons and debug builds at once).
const HELD_LEASE: &str = "15s";

fn short_leases(config: &mut Config) {
    let lease = &mut config.file.lease;
    lease.default = Duration::from_secs(4);
    lease.max = Duration::from_secs(60);
    lease.grace = Duration::from_secs(2);
    lease.heartbeat = Duration::from_secs(1);
    lease.tick = Duration::from_millis(500);
}

async fn daemon() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, short_leases).await
}

/// `reef --no-autostart` against `daemon`, with a clean environment and
/// stdin from `/dev/null`.
fn reef(daemon: &TestDaemon) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_reef"));
    for var in [
        "REEF_SOCKET",
        "REEF_CONFIG",
        "REEF_LOG",
        "RUST_LOG",
        "REEF_LOG_FORMAT",
        "REEF_AGENT",
        "REEF_WORKSPACE",
        "REEF_REMOTE",
    ] {
        command.env_remove(var);
    }
    command
        .env("REEF_STATE_DIR", daemon.state_dir())
        .arg("--no-autostart")
        .stdin(Stdio::null());
    command
}

/// Runs `reef <args>` to completion off the runtime's worker threads,
/// which the in-process daemon needs.
async fn run(daemon: &TestDaemon, args: &[&str]) -> Output {
    let mut command = reef(daemon);
    command.args(args);
    finish(command).await
}

async fn finish(mut command: std::process::Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

/// `reef -o json <args>`, which must succeed, as JSON.
async fn ok(daemon: &TestDaemon, args: &[&str]) -> Value {
    let mut line = vec!["-o", "json"];
    line.extend_from_slice(args);
    let output = run(daemon, &line).await;
    assert!(
        output.status.success(),
        "reef {line:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    common::schema::check(&line, &output);
    serde_json::from_slice(&output.stdout).unwrap()
}

/// `reef -o json <args>`, which must fail with `code`: its error object.
async fn fails(daemon: &TestDaemon, args: &[&str], code: i32) -> Value {
    let mut line = vec!["-o", "json"];
    line.extend_from_slice(args);
    let output = run(daemon, &line).await;
    assert_eq!(
        output.status.code(),
        Some(code),
        "reef {line:?}: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    common::schema::check(&line, &output);
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    value["error"].clone()
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_owned()
}

/// Processes standing in for a program's sessions, killed on drop.
#[derive(Default)]
struct Holders(Vec<Child>);

impl Holders {
    fn spawn(&mut self) -> i32 {
        let child = std::process::Command::new("sleep")
            .arg("600")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        self.0.push(child);
        pid
    }

    fn kill(&mut self, pid: i32) {
        if let Some(child) = self
            .0
            .iter_mut()
            .find(|child| child.id() == pid.unsigned_abs())
        {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

impl Drop for Holders {
    fn drop(&mut self) {
        for child in &mut self.0 {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Every follower a test's daemon has a record of, killed on drop, so a
/// failed test leaks none.
struct Followers(PathBuf);

impl Followers {
    fn of(daemon: &TestDaemon) -> Self {
        Self(daemon.state_dir().join("holders"))
    }
}

impl Drop for Followers {
    fn drop(&mut self) {
        let Ok(entries) = std::fs::read_dir(&self.0) else {
            return;
        };
        for entry in entries.filter_map(Result::ok) {
            let pid = std::fs::read(entry.path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .and_then(|record| record["follower"]["pid"].as_i64())
                .and_then(|pid| i32::try_from(pid).ok());
            if let Some(pid) = pid.filter(|pid| *pid > 1) {
                let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            }
        }
    }
}

/// A record of `id`'s, any follower's.
fn record(daemon: &TestDaemon, id: &str) -> Option<Value> {
    let entries = std::fs::read_dir(daemon.state_dir().join("holders")).ok()?;
    entries.filter_map(Result::ok).find_map(|entry| {
        let name = entry.file_name().to_string_lossy().into_owned();
        let json = Path::new(&name)
            .extension()
            .is_some_and(|extension| extension == "json");
        if !(name.starts_with(&format!("{id}.")) && json) {
            return None;
        }
        serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()
    })
}

fn running(pid: i64) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // A zombie answers kill(0); `ps` tells it apart where it exists.
    if kill(Pid::from_raw(pid), None).is_err() {
        return false;
    }
    match std::process::Command::new("ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
    {
        Ok(out) => {
            let stat = String::from_utf8_lossy(&out.stdout);
            !stat.trim().is_empty() && !stat.trim_start().starts_with('Z')
        }
        Err(_) => true,
    }
}

/// Waits for a follower's record of `id`, showing its log if none comes.
async fn wait_record(daemon: &TestDaemon, id: &str) {
    let deadline = Instant::now() + RECORD_WITHIN;
    while record(daemon, id).is_none() {
        if Instant::now() >= deadline {
            let log = daemon
                .state_dir()
                .join("logs")
                .join(format!("follower-{id}.log"));
            panic!(
                "no holder record for {id} within {RECORD_WITHIN:?}; follower log: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Waits up to `within` for `done`.
async fn until(within: Duration, what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + within;
    while !done() {
        assert!(Instant::now() < deadline, "not within {within:?}: {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn inspect(daemon: &TestDaemon, id: &str) -> Value {
    ok(daemon, &["ws", "inspect", "--no-snapshot", id]).await
}

/// Waits for `id` to reach `state`.
async fn until_state(daemon: &TestDaemon, id: &str, state: &str, within: Duration) -> Value {
    let deadline = Instant::now() + within;
    loop {
        let ws = inspect(daemon, id).await;
        if ws["state"] == state {
            return ws;
        }
        assert!(Instant::now() < deadline, "{id} not {state}: {ws}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn create_attach_release() {
    let (daemon, repo) = daemon().await;
    let _followers = Followers::of(&daemon);
    let mut holders = Holders::default();
    let holder = holders.spawn();
    let pid = holder.to_string();
    let create = [
        "ws",
        "provider",
        "create",
        "--repo",
        &repo.id,
        "--name",
        "t1",
        "--lease",
        HELD_LEASE,
        "--hold-while",
        &pid,
    ];
    let created = ok(&daemon, &create).await;
    assert_eq!(created["contract"], 1, "{created}");
    assert_eq!(created["attached"], false);
    assert_eq!(created["holder"]["pid"], holder);
    assert!(text(&created["owner"]).ends_with(":t1"), "{created}");
    assert_eq!(created["purpose"], "t1");
    let path = PathBuf::from(text(&created["path"]));
    assert!(path.is_dir() && !path.starts_with(&repo.root), "{path:?}");
    let id = text(&created["workspace_id"]);
    let follower = created["holder"]["follower_pid"].as_i64().unwrap();

    // The follower records itself after its first renewal, and keeps
    // the lease moving.
    wait_record(&daemon, &id).await;
    let first = text(&inspect(&daemon, &id).await["lease"]["expires_at"]);
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let later = text(&inspect(&daemon, &id).await["lease"]["expires_at"]);
    assert!(later > first, "{first} then {later}");

    // The same name: the same workspace, and no second follower.
    let again = ok(&daemon, &create).await;
    assert_eq!(again["workspace_id"], created["workspace_id"]);
    assert_eq!(again["attached"], true);
    assert_eq!(again["holder"]["follower_pid"], follower);

    let released = ok(&daemon, &["ws", "provider", "release", "--workspace", &id]).await;
    assert_eq!(released["released"], true, "{released}");
    assert_eq!(released["snapshot_taken"], true);
    until(Duration::from_secs(2), "the follower to exit", || {
        !running(follower)
    })
    .await;
    assert!(record(&daemon, &id).is_none());

    // Again by id: not an error. By path: gone.
    let twice = ok(&daemon, &["ws", "provider", "release", "--workspace", &id]).await;
    assert_eq!(twice["released"], false, "{twice}");
    assert_eq!(twice["state"], "released");
    let by_path = path.display().to_string();
    fails(
        &daemon,
        &["ws", "provider", "release", "--workspace", &by_path],
        3,
    )
    .await;
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn holder_death_reclaims() {
    let (daemon, repo) = daemon().await;
    let _followers = Followers::of(&daemon);
    let mut holders = Holders::default();
    let holder = holders.spawn();
    let created = ok(
        &daemon,
        &[
            "ws",
            "provider",
            "create",
            "--repo",
            &repo.id,
            "--lease",
            HELD_LEASE,
            "--hold-while",
            &holder.to_string(),
        ],
    )
    .await;
    let id = text(&created["workspace_id"]);
    let follower = created["holder"]["follower_pid"].as_i64().unwrap();
    wait_record(&daemon, &id).await;
    std::fs::write(
        Path::new(&text(&created["path"])).join("held.txt"),
        "kept\n",
    )
    .unwrap();

    holders.kill(holder);
    until(Duration::from_secs(3), "the follower to exit", || {
        !running(follower)
    })
    .await;
    assert!(record(&daemon, &id).is_none());
    // Length, grace, a tick, and room for a slow machine.
    let reclaimed = until_state(&daemon, &id, "reclaimed", Duration::from_secs(30)).await;
    let commit = text(&reclaimed["commit_id"]);
    let read = ok(
        &daemon,
        &[
            "file", "read", "--repo", &repo.id, "-r", &commit, "held.txt",
        ],
    )
    .await;
    assert_eq!(read["content"], "kept\n", "{read}");
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn attach_after_restart() {
    let (daemon, repo) = daemon().await;
    let _followers = Followers::of(&daemon);
    let mut holders = Holders::default();
    let first = holders.spawn();
    let created = ok(
        &daemon,
        &[
            "ws",
            "provider",
            "create",
            "--repo",
            &repo.id,
            "--lease",
            HELD_LEASE,
            "--hold-while",
            &first.to_string(),
        ],
    )
    .await;
    let id = text(&created["workspace_id"]);
    let path = text(&created["path"]);
    let old_follower = created["holder"]["follower_pid"].as_i64().unwrap();
    wait_record(&daemon, &id).await;
    holders.kill(first);
    until(Duration::from_secs(3), "the first follower to exit", || {
        !running(old_follower)
    })
    .await;

    // Within the lease, a new holder takes it over and keeps it past the
    // first lease's end.
    let second = holders.spawn();
    let attached = ok(
        &daemon,
        &[
            "ws",
            "provider",
            "attach",
            "--workspace",
            &path,
            "--hold-while",
            &second.to_string(),
        ],
    )
    .await;
    assert_eq!(
        attached["workspace_id"], created["workspace_id"],
        "{attached}"
    );
    assert_eq!(attached["attached"], true);
    assert_eq!(attached["holder"]["pid"], second);
    // Past the first lease (15 s) and its grace (2 s).
    tokio::time::sleep(Duration::from_secs(18)).await;
    assert_eq!(inspect(&daemon, &id).await["state"], "active");

    // From stdin, the same holder: the follower is kept.
    let mut command = reef(&daemon);
    command
        .args([
            "-o",
            "json",
            "ws",
            "provider",
            "attach",
            "--stdin",
            "--hold-while",
        ])
        .arg(second.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped());
    let mut child = command.spawn().unwrap();
    std::io::Write::write_all(
        child.stdin.as_mut().unwrap(),
        json!({"workspace": id}).to_string().as_bytes(),
    )
    .unwrap();
    drop(child.stdin.take());
    let output = tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap();
    assert!(output.status.success());
    let from_stdin: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        from_stdin["holder"]["follower_pid"],
        attached["holder"]["follower_pid"]
    );

    ok(&daemon, &["ws", "provider", "release", "--workspace", &id]).await;
}

// 8, continued
#[tokio::test(flavor = "multi_thread")]
async fn attach_refusals() {
    let (daemon, repo) = daemon().await;
    let _followers = Followers::of(&daemon);
    let created = ok(
        &daemon,
        &[
            "ws", "provider", "create", "--repo", &repo.id, "--lease", "60s",
        ],
    )
    .await;
    let id = text(&created["workspace_id"]);
    // Work, so the change outlives the release: an empty one is dropped.
    std::fs::write(
        Path::new(&text(&created["path"])).join("work.txt"),
        "work\n",
    )
    .unwrap();
    // Released: refused, with a `next` that starts again on its change.
    ok(&daemon, &["ws", "provider", "release", "--workspace", &id]).await;
    let error = fails(
        &daemon,
        &["ws", "provider", "attach", "--workspace", &id],
        5,
    )
    .await;
    let next: Vec<String> = serde_json::from_value(error["next"].clone()).unwrap();
    assert_eq!(next[..4], ["reef", "ws", "create", "--repo"], "{error}");
    let words: Vec<&str> = next[1..].iter().map(String::as_str).collect();
    let again = ok(&daemon, &words).await;
    ok(&daemon, &["ws", "release", &text(&again["id"])]).await;

    // An empty change is dropped with its workspace: start afresh.
    let empty = ok(
        &daemon,
        &[
            "ws", "provider", "create", "--repo", &repo.id, "--lease", "60s",
        ],
    )
    .await;
    let empty_id = text(&empty["workspace_id"]);
    ok(
        &daemon,
        &["ws", "provider", "release", "--workspace", &empty_id],
    )
    .await;
    let error = fails(
        &daemon,
        &["ws", "provider", "attach", "--workspace", &empty_id],
        5,
    )
    .await;
    let next: Vec<String> = serde_json::from_value(error["next"].clone()).unwrap();
    assert_eq!(
        next,
        ["reef", "ws", "create", "--repo", repo.id.as_str()],
        "{error}"
    );
    assert!(text(&error["hint"]).contains("empty"), "{error}");

    // Another owner's: refused, naming --owner.
    let theirs = ok(
        &daemon,
        &[
            "ws",
            "create",
            "--repo",
            &repo.id,
            "--owner",
            "someone@elsewhere",
        ],
    )
    .await;
    let error = fails(
        &daemon,
        &[
            "ws",
            "provider",
            "attach",
            "--workspace",
            &text(&theirs["id"]),
        ],
        5,
    )
    .await;
    assert!(text(&error["message"]).contains("--owner"), "{error}");
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn nested_create_starts_on_the_parent() {
    let (daemon, repo) = daemon().await;
    let _followers = Followers::of(&daemon);
    let outer = ok(
        &daemon,
        &[
            "ws", "provider", "create", "--repo", &repo.id, "--lease", "60s",
        ],
    )
    .await;
    let outer_path = PathBuf::from(text(&outer["path"]));
    std::fs::write(outer_path.join("nested.txt"), "from the parent\n").unwrap();
    let inner = ok(
        &daemon,
        &[
            "ws",
            "provider",
            "create",
            "--repo",
            &outer_path.display().to_string(),
            "--lease",
            "60s",
        ],
    )
    .await;
    assert_ne!(inner["workspace_id"], outer["workspace_id"]);
    let inner_path = PathBuf::from(text(&inner["path"]));
    assert_eq!(
        std::fs::read_to_string(inner_path.join("nested.txt")).unwrap(),
        "from the parent\n"
    );
    let outer_now = inspect(&daemon, &text(&outer["workspace_id"])).await;
    let shown = ok(
        &daemon,
        &["show", "--workspace", &text(&inner["workspace_id"])],
    )
    .await;
    assert!(
        shown.to_string().contains(&text(&outer_now["commit_id"])),
        "the inner change's parent is the outer's commit: {shown}"
    );
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn errors_are_documents() {
    let (daemon, repo) = daemon().await;
    let _followers = Followers::of(&daemon);
    let count = |list: &Value| list.as_array().map_or(0, Vec::len);
    let before = count(&ok(&daemon, &["ws", "list", "--all"]).await);

    let nowhere = tempfile::tempdir().unwrap();
    let error = fails(
        &daemon,
        &[
            "ws",
            "provider",
            "create",
            "--repo",
            &nowhere.path().display().to_string(),
        ],
        3,
    )
    .await;
    assert!(
        error["kind"].is_string() && error["message"].is_string(),
        "{error}"
    );

    let error = fails(
        &daemon,
        &[
            "ws",
            "provider",
            "create",
            "--repo",
            &repo.id,
            "--lease",
            HELD_LEASE,
            "--hold-while",
            "999999",
        ],
        2,
    )
    .await;
    assert_eq!(error["reason"], "HOLDER_NOT_RUNNING", "{error}");
    assert_eq!(error["metadata"]["field"], "hold_while");

    let error = fails(&daemon, &["ws", "provider", "attach", "--stdin"], 2).await;
    assert_eq!(error["reason"], "PROVIDER_INPUT_INVALID", "{error}");

    let error = fails(&daemon, &["ws", "provider", "attach"], 2).await;
    assert_eq!(error["metadata"]["field"], "workspace", "{error}");

    // Local only: a remote daemon's directories are not this machine's.
    let error = fails(
        &daemon,
        &["--remote", "elsewhere", "ws", "provider", "create"],
        9,
    )
    .await;
    assert!(text(&error["message"]).contains("ws provider"), "{error}");

    // A follower that cannot start: the workspace just made is released.
    // File modes do not stop root, so only another user can check it.
    if nix::unistd::geteuid().is_root() {
        return;
    }
    let logs = daemon.state_dir().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let mode = |mode| {
        std::fs::set_permissions(&logs, std::os::unix::fs::PermissionsExt::from_mode(mode))
            .unwrap();
    };
    mode(0o500);
    let mut holders = Holders::default();
    let holder = holders.spawn().to_string();
    let output = run(
        &daemon,
        &[
            "-o",
            "json",
            "ws",
            "provider",
            "create",
            "--repo",
            &repo.id,
            "--hold-while",
            &holder,
        ],
    )
    .await;
    mode(0o700);
    assert_eq!(output.status.code(), Some(8), "{output:?}");
    common::schema::check(&["-o", "json", "ws", "provider", "create"], &output);
    // Released, or on its way: nothing is left active.
    let listed = ok(&daemon, &["ws", "list"]).await;
    let active = listed
        .as_array()
        .unwrap()
        .iter()
        .filter(|ws| ws["state"] == "active")
        .count();
    assert_eq!(active, 0, "{listed}");
    assert_eq!(
        count(&ok(&daemon, &["ws", "list", "--all"]).await),
        before + 1
    );
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn here_respects_the_holder() {
    let (daemon, repo) = daemon().await;
    let _followers = Followers::of(&daemon);
    let mut holders = Holders::default();
    let holder = holders.spawn();
    let held = ok(
        &daemon,
        &[
            "ws",
            "provider",
            "create",
            "--repo",
            &repo.id,
            "--lease",
            HELD_LEASE,
            "--hold-while",
            &holder.to_string(),
        ],
    )
    .await;
    let id = text(&held["workspace_id"]);
    wait_record(&daemon, &id).await;
    let here = |dir: String| {
        let mut command = reef(&daemon);
        command.current_dir(dir).args(["-o", "json", "here"]);
        finish(command)
    };
    let inside: Value = serde_json::from_slice(&here(text(&held["path"])).await.stdout).unwrap();
    assert_eq!(inside["held"], true, "{inside}");
    assert_eq!(inside["next"], json!([]));
    assert!(
        text(&inside["hint"]).contains("program that created"),
        "{inside}"
    );

    // The holder gone, its follower gone: the record is stale, and not
    // held any more.
    let follower = held["holder"]["follower_pid"].as_i64().unwrap();
    holders.kill(holder);
    until(Duration::from_secs(4), "the follower to exit", || {
        !running(follower)
    })
    .await;
    let stale: Value = serde_json::from_slice(&here(text(&held["path"])).await.stdout).unwrap();
    assert_eq!(stale["held"], false, "{stale}");

    // Unheld: back to releasing it when done.
    let free = ok(
        &daemon,
        &[
            "ws", "provider", "create", "--repo", &repo.id, "--lease", "1m",
        ],
    )
    .await;
    let outside: Value = serde_json::from_slice(&here(text(&free["path"])).await.stdout).unwrap();
    assert_eq!(outside["held"], false, "{outside}");
    assert_eq!(outside["next"][2], "release", "{outside}");
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn renew_follow() {
    let (daemon, repo) = daemon().await;
    let _followers = Followers::of(&daemon);
    let created = ok(
        &daemon,
        &["ws", "create", "--repo", &repo.id, "--lease", "60s"],
    )
    .await;
    let id = text(&created["id"]);
    let follow = |pid: Option<i32>| {
        let mut command = reef(&daemon);
        command.args(["ws", "renew", "--follow"]);
        if let Some(pid) = pid {
            command.args(["--while-pid", &pid.to_string()]);
        }
        command
            .arg(&id)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    };
    let exits = |child: Child, within: Duration| async move {
        let deadline = Instant::now() + within;
        let mut child = child;
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                let output = child.wait_with_output().unwrap();
                return (status, output.stdout);
            }
            assert!(Instant::now() < deadline, "the follower did not exit");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };

    // The holder exits: so does the follower, with nothing on stdout.
    let mut holders = Holders::default();
    let holder = holders.spawn();
    let child = follow(Some(holder));
    wait_record(&daemon, &id).await;
    holders.kill(holder);
    let (status, stdout) = exits(child, Duration::from_secs(4)).await;
    assert!(status.success() && stdout.is_empty(), "{status:?}");
    assert!(record(&daemon, &id).is_none());

    // SIGTERM.
    let holder = holders.spawn();
    let child = follow(Some(holder));
    wait_record(&daemon, &id).await;
    kill(
        Pid::from_raw(i32::try_from(child.id()).unwrap()),
        Signal::SIGTERM,
    )
    .unwrap();
    let (status, _) = exits(child, Duration::from_secs(4)).await;
    assert!(status.success(), "{status:?}");

    // No holder: until the workspace is released.
    let child = follow(None);
    wait_record(&daemon, &id).await;
    ok(&daemon, &["ws", "release", &id]).await;
    let (status, _) = exits(child, Duration::from_secs(5)).await;
    assert!(status.success(), "{status:?}");

    // --while-pid needs --follow.
    let output = run(&daemon, &["ws", "renew", "--while-pid", "1", &id]).await;
    assert_eq!(output.status.code(), Some(2));
}

/// The contract document, and the commands' help, name no harness: an
/// adapter is where a harness is named (R2.26, the agent-agnostic rule).
#[test]
fn the_contract_names_no_harness() {
    let doc = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/workspace-provider.md"),
    )
    .unwrap();
    let mut texts = vec![doc];
    for words in [
        &["ws", "provider", "--help"][..],
        &["ws", "provider", "create", "--help"],
        &["ws", "provider", "attach", "--help"],
        &["ws", "provider", "release", "--help"],
        &["ws", "renew", "--help"],
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_reef"))
            .args(words)
            .output()
            .unwrap();
        assert!(output.status.success(), "{words:?}");
        texts.push(String::from_utf8(output.stdout).unwrap());
    }
    for text in texts {
        let lower = text.to_lowercase();
        for harness in [
            "claude",
            "codex",
            "cursor",
            "opencode",
            "gemini",
            "anthropic",
            "openai",
            "gpt",
            "worktreecreate",
        ] {
            assert!(!lower.contains(harness), "{harness}: {text}");
        }
    }
}
