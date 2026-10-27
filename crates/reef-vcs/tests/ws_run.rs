//! `reef ws run` against a daemon (R1.21). Numbered as the brief numbers
//! them; test 13, the heartbeater's backoff, is a unit test in
//! `src/cli/ws_run.rs`.
//!
//! Each test runs a [`TestDaemon`] in this process with a `small`
//! synthetic repository and the `reef` binary against its socket with
//! `--no-autostart`. Lease tests use the brief's short configuration:
//! `lease.default = 2s`, `grace = 1s`, `heartbeat = 500ms`, `tick = 500ms`.
//! Commands run with stdin from `/dev/null`, the non-terminal case; the
//! terminal case is checked by hand (see the PR).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use std::time::{Duration, Instant};

use jj_lib::backend::CommitId;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use nix::sys::signal::{Signal, kill};
use nix::unistd::Pid;
use reef_daemon::config::Config;
use reef_daemon::model::{ReclaimReason, ReclaimRecord, WorkspaceId};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, test_settings};
use reef_lib::jj::Repo;
use reef_testing::synth::Preset;
use serde_json::Value;

/// How long a test waits for a reclaim it expects: length, grace, and a
/// tick, with room for a slow CI machine.
const RECLAIM_BOUND: Duration = Duration::from_secs(15);

fn short_leases(config: &mut Config) {
    let lease = &mut config.file.lease;
    lease.default = Duration::from_secs(2);
    lease.max = Duration::from_secs(10);
    lease.grace = Duration::from_secs(1);
    lease.heartbeat = Duration::from_millis(500);
    lease.tick = Duration::from_millis(500);
}

async fn with_repo() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo(Preset::Small).await
}

async fn with_short_leases() -> (TestDaemon, SyntheticRepo) {
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
    ] {
        command.env_remove(var);
    }
    command
        .env("REEF_STATE_DIR", daemon.state_dir())
        .arg("--no-autostart")
        .stdin(Stdio::null());
    command
}

/// `reef ws run --repo <repo> <args>`, not yet started.
fn ws_run(daemon: &TestDaemon, repo: &SyntheticRepo, args: &[&str]) -> std::process::Command {
    let mut command = reef(daemon);
    command.args(["ws", "run", "--repo", &repo.id]).args(args);
    command
}

/// Runs `command` to completion off the runtime's worker threads, which
/// the in-process daemon needs.
async fn finish(mut command: std::process::Command) -> Output {
    tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap()
}

/// Starts `command` with stdout and stderr piped.
fn spawn(mut command: std::process::Command) -> Child {
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap()
}

/// Waits for a started command off the runtime's worker threads.
async fn wait(child: Child) -> Output {
    tokio::task::spawn_blocking(move || child.wait_with_output().unwrap())
        .await
        .unwrap()
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn code(output: &Output) -> i32 {
    output
        .status
        .code()
        .unwrap_or_else(|| panic!("killed: {:?}\n{}", output.status, stderr(output)))
}

fn pid_of(child: &Child) -> Pid {
    Pid::from_raw(i32::try_from(child.id()).unwrap())
}

/// Every workspace the daemon knows, in any state.
async fn workspaces(daemon: &TestDaemon) -> Vec<Value> {
    let output = finish({
        let mut command = reef(daemon);
        command.args(["-o", "json", "ws", "list", "--all"]);
        command
    })
    .await;
    assert!(output.status.success(), "{}", stderr(&output));
    serde_json::from_str::<Vec<Value>>(&stdout(&output)).unwrap()
}

/// The only workspace the daemon knows.
async fn only_workspace(daemon: &TestDaemon) -> Value {
    let mut all = workspaces(daemon).await;
    assert_eq!(all.len(), 1, "{all:?}");
    all.pop().unwrap()
}

async fn workspace(daemon: &TestDaemon, id: &str) -> Value {
    workspaces(daemon)
        .await
        .into_iter()
        .find(|ws| ws["id"] == id)
        .unwrap_or_else(|| panic!("no workspace {id}"))
}

/// Polls `check` every 50 ms until it holds, failing after `bound`.
async fn wait_for<F, Fut>(bound: Duration, what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let begun = Instant::now();
    while !check().await {
        assert!(begun.elapsed() < bound, "{what}: not within {bound:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Waits for `path` to exist.
async fn wait_for_file(path: &Path) {
    wait_for(RECLAIM_BOUND, &format!("{}", path.display()), || async {
        path.exists()
    })
    .await;
}

fn reclaims_of(daemon: &TestDaemon, id: &str) -> Vec<ReclaimRecord> {
    let id: WorkspaceId = id.parse().unwrap();
    daemon
        .handle
        .state()
        .db
        .reclaims(None, 100)
        .unwrap()
        .into_iter()
        .filter(|row| row.workspace_id == id)
        .collect()
}

/// Whether `path` is in `commit`'s tree, read through the registered
/// root's own jj workspace.
async fn tree_has(root: &Path, commit: &str, path: &str) -> bool {
    let (root, commit, path) = (root.to_owned(), commit.to_owned(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let repo = pollster::block_on(Repo::open(&root, &test_settings())).unwrap();
        let id = CommitId::try_from_hex(&commit).unwrap();
        let commit = repo.head().store().get_commit(&id).unwrap();
        let path = RepoPathBuf::from_internal_string(path).unwrap();
        !pollster::block_on(commit.tree().path_value(&path))
            .unwrap()
            .is_absent()
    })
    .await
    .unwrap()
}

/// A path for a file a test's command writes to say where it got to,
/// outside the workspace so it is not snapshotted.
fn signal_file(daemon: &TestDaemon, name: &str) -> PathBuf {
    daemon.tmp.path().join(name)
}

fn env_arg(name: &str, path: &Path) -> String {
    format!("{name}={}", path.display())
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn runs_command_in_workspace() {
    let (daemon, repo) = with_repo().await;
    let script = r#"pwd; echo "$REEF_WORKSPACE"; echo "$REEF_REPO"; echo "$REEF_SOCKET"; echo "$REEF_WORKSPACE_PATH"; echo "$REEF_WORKSPACE" > marker"#;
    let output = finish(ws_run(&daemon, &repo, &["--", "sh", "-c", script])).await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 5, "{text}");
    let (cwd, id) = (lines[0], lines[1]);
    let ws_root = |state: &Path| state.join("repos").join(&repo.id).join("ws").join(id);
    let state = daemon.state_dir();
    assert!(
        Path::new(cwd) == ws_root(state)
            || Path::new(cwd) == ws_root(&std::fs::canonicalize(state).unwrap()),
        "{cwd} is not under {}",
        state.display()
    );
    assert_eq!(lines[2], repo.id);
    assert_eq!(Path::new(lines[3]), daemon.config.paths.socket);
    assert_eq!(lines[4], cwd);
    // Nothing of `ws run`'s own on stdout in text mode.
    let ws = workspace(&daemon, id).await;
    assert_eq!(ws["state"], "released", "{ws}");
    assert!(!Path::new(cwd).exists());
    let commit = ws["commit_id"].as_str().unwrap();
    assert!(tree_has(&repo.root, commit, "marker").await);
}

/// R2.05 test 11: `ws run` passes the agent on to its command as
/// `REEF_AGENT`, and the operations it causes name it; with no agent the
/// variable is unset.
#[tokio::test(flavor = "multi_thread")]
async fn ws_run_exports_the_agent() {
    use reef_api::v1::{ListOperationsRequest, OperationFilter, OperationKind};
    let (daemon, repo) = with_repo().await;
    let script = r#"echo "${REEF_AGENT-unset}"; echo "$REEF_WORKSPACE"; echo x > agent-file"#;
    let mut command = reef(&daemon);
    command
        .args([
            "--agent", "bot-3", "ws", "run", "--repo", &repo.id, "--", "sh", "-c",
        ])
        .arg(script);
    let output = finish(command).await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let text = stdout(&output);
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[0], "bot-3", "{text}");
    let client = reef_client::Client::connect(&daemon.config.paths.socket)
        .await
        .unwrap();
    let page = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            filter: Some(OperationFilter {
                workspaces: vec![lines[1].to_owned()],
                kinds: vec![OperationKind::Snapshot.into()],
                ..OperationFilter::default()
            }),
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap();
    assert!(!page.operations.is_empty(), "{page:?}");
    for operation in &page.operations {
        assert_eq!(operation.agent, "bot-3", "{operation:?}");
    }

    let output = finish(ws_run(&daemon, &repo, &["--", "sh", "-c", script])).await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output).lines().next(), Some("unset"));
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn exit_code_is_the_childs() {
    let (daemon, repo) = with_repo().await;
    let output = finish(ws_run(&daemon, &repo, &["--", "sh", "-c", "exit 7"])).await;
    assert_eq!(code(&output), 7, "{}", stderr(&output));
    let output = finish(ws_run(&daemon, &repo, &["--", "sh", "-c", "kill -TERM $$"])).await;
    assert_eq!(code(&output), 143, "{}", stderr(&output));
    for ws in workspaces(&daemon).await {
        assert_eq!(ws["state"], "released", "{ws}");
    }
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn missing_command() {
    let (daemon, repo) = with_repo().await;
    let output = finish(ws_run(&daemon, &repo, &["--", "does-not-exist"])).await;
    assert_eq!(code(&output), 127);
    assert!(
        stderr(&output).contains("reef: cannot run does-not-exist"),
        "{}",
        stderr(&output)
    );
    assert_eq!(only_workspace(&daemon).await["state"], "released");
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn heartbeats_keep_the_lease() {
    let (daemon, repo) = with_short_leases().await;
    // The command runs until the test says stop, so a slow start under a
    // parallel run cannot use up the time the test watches the lease for.
    let stop = signal_file(&daemon, "stop");
    let script = r#"while [ ! -e "$STOP" ]; do sleep 0.1; done"#;
    let child = spawn(ws_run(
        &daemon,
        &repo,
        &["--env", &env_arg("STOP", &stop), "--", "sh", "-c", script],
    ));
    wait_for(RECLAIM_BOUND, "workspace created", || async {
        !workspaces(&daemon).await.is_empty()
    })
    .await;
    let ws = only_workspace(&daemon).await;
    let id = ws["id"].as_str().unwrap().to_owned();
    let first = ws["lease"]["expires_at"].clone();
    wait_for(RECLAIM_BOUND, "lease renewed", || async {
        workspace(&daemon, &id).await["lease"]["expires_at"] != first
    })
    .await;
    // Past the 2 s lease and 1 s grace from the last sighting of the first
    // expiry: only heartbeats can have kept it active.
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert_eq!(workspace(&daemon, &id).await["state"], "active");
    std::fs::write(&stop, "").unwrap();
    let output = wait(child).await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(workspace(&daemon, &id).await["state"], "released");
    let reclaims = reclaims_of(&daemon, &id);
    assert_eq!(reclaims.len(), 1, "{reclaims:?}");
    assert_eq!(reclaims[0].reason, ReclaimReason::Released);
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn killing_ws_run_reclaims() {
    let (daemon, repo) = with_short_leases().await;
    let pid_file = signal_file(&daemon, "pid");
    let script = r#"echo hi > f; echo $$ > "$PID_FILE"; exec sleep 60"#;
    let child = spawn(ws_run(
        &daemon,
        &repo,
        &[
            "--env",
            &env_arg("PID_FILE", &pid_file),
            "--",
            "sh",
            "-c",
            script,
        ],
    ));
    wait_for_file(&pid_file).await;
    // The pid is written after `f`, and before the `exec`, which keeps it.
    let sleep: i32 = loop {
        if let Ok(pid) = std::fs::read_to_string(&pid_file).unwrap().trim().parse() {
            break pid;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    kill(pid_of(&child), Signal::SIGKILL).unwrap();
    // The orphaned `sleep` holds the pipes; it is not under test.
    kill(Pid::from_raw(sleep), Signal::SIGKILL).unwrap();
    let _ = wait(child).await;
    let id = only_workspace(&daemon).await["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for(RECLAIM_BOUND, "workspace reclaimed", || async {
        workspace(&daemon, &id).await["state"] == "reclaimed"
    })
    .await;
    let ws = workspace(&daemon, &id).await;
    assert!(!Path::new(ws["path"].as_str().unwrap()).exists());
    let reclaims = reclaims_of(&daemon, &id);
    assert_eq!(reclaims.len(), 1, "{reclaims:?}");
    assert_eq!(reclaims[0].reason, ReclaimReason::Expired);
    assert!(reclaims[0].snapshot_taken);
    let commit = ws["commit_id"].as_str().unwrap();
    assert!(tree_has(&repo.root, commit, "f").await);
}

/// Runs `script` under `ws run` with `$READY` set to a file it must create
/// once its traps are set, and returns the running `ws run` after that.
async fn run_until_ready(daemon: &TestDaemon, repo: &SyntheticRepo, script: &str) -> Child {
    let ready = signal_file(daemon, "ready");
    let child = spawn(ws_run(
        daemon,
        repo,
        &["--env", &env_arg("READY", &ready), "--", "sh", "-c", script],
    ));
    wait_for_file(&ready).await;
    child
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn sigterm_is_forwarded() {
    let (daemon, repo) = with_repo().await;
    let script = r#"trap 'echo caught; kill $!; exit 9' TERM; : > "$READY"; sleep 60 >/dev/null 2>&1 & wait"#;
    let child = run_until_ready(&daemon, &repo, script).await;
    kill(pid_of(&child), Signal::SIGTERM).unwrap();
    let output = wait(child).await;
    assert_eq!(code(&output), 9, "{}", stderr(&output));
    assert!(stdout(&output).contains("caught"), "{}", stdout(&output));
    assert_eq!(only_workspace(&daemon).await["state"], "released");
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn sigint_forwarded_without_terminal() {
    let (daemon, repo) = with_repo().await;
    let script = r#"trap 'echo caught; kill $!; exit 11' INT; : > "$READY"; sleep 60 >/dev/null 2>&1 & wait"#;
    let child = run_until_ready(&daemon, &repo, script).await;
    kill(pid_of(&child), Signal::SIGINT).unwrap();
    let output = wait(child).await;
    assert_eq!(code(&output), 11, "{}", stderr(&output));
    assert!(stdout(&output).contains("caught"), "{}", stdout(&output));
    assert_eq!(only_workspace(&daemon).await["state"], "released");
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn escalates_to_sigkill() {
    let (daemon, repo) = with_repo().await;
    // `exec` keeps the ignored TERM and leaves no orphan holding the pipes.
    let script = r#"trap '' TERM; : > "$READY"; exec sleep 60"#;
    let child = run_until_ready(&daemon, &repo, script).await;
    let begun = Instant::now();
    kill(pid_of(&child), Signal::SIGTERM).unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    kill(pid_of(&child), Signal::SIGTERM).unwrap();
    let output = wait(child).await;
    assert!(
        begun.elapsed() < Duration::from_secs(7),
        "{:?}",
        begun.elapsed()
    );
    assert_eq!(code(&output), 137, "{}", stderr(&output));
    assert_eq!(only_workspace(&daemon).await["state"], "released");
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn keep_leaves_workspace_active() {
    let (daemon, repo) = with_short_leases().await;
    // So `--keep`'s snapshot is the one that records the write.
    daemon.handle.state().manager.scheduler().pause_watcher();
    let output = finish(ws_run(
        &daemon,
        &repo,
        &["-o", "json", "--keep", "--", "sh", "-c", "echo kept > kept"],
    ))
    .await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(report["kept"], true, "{report}");
    assert_eq!(report["released"], false, "{report}");
    assert!(report["final_op_id"].is_string(), "{report}");
    let id = report["workspace_id"].as_str().unwrap().to_owned();
    let ws = workspace(&daemon, &id).await;
    assert_eq!(ws["state"], "active", "{ws}");
    assert_eq!(ws["last_snapshot_op"], report["final_op_id"], "{ws}");
    // No heartbeats after the command: the lease runs out.
    wait_for(RECLAIM_BOUND, "workspace reclaimed", || async {
        workspace(&daemon, &id).await["state"] == "reclaimed"
    })
    .await;
    assert_eq!(reclaims_of(&daemon, &id)[0].reason, ReclaimReason::Expired);
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn attach_existing_workspace() {
    let (daemon, repo) = with_repo().await;
    let created = finish({
        let mut command = reef(&daemon);
        command.args(["-o", "json", "ws", "create", "--repo", &repo.id]);
        command
    })
    .await;
    assert!(created.status.success(), "{}", stderr(&created));
    let created: Value = serde_json::from_str(&stdout(&created)).unwrap();
    let id = created["id"].as_str().unwrap();
    let attach = || {
        let mut command = reef(&daemon);
        command.args(["ws", "run", "--workspace", id, "--", "pwd"]);
        command
    };
    let output = finish(attach()).await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), created["path"].as_str().unwrap());
    assert_eq!(workspace(&daemon, id).await["state"], "released");
    let output = finish(attach()).await;
    assert_eq!(code(&output), 5, "{}", stderr(&output));
    assert!(stderr(&output).contains("released"), "{}", stderr(&output));
}

// 10, the owner rule: another owner's workspace needs `--owner`.
#[tokio::test(flavor = "multi_thread")]
async fn attach_checks_the_owner() {
    let (daemon, repo) = with_repo().await;
    let created = finish({
        let mut command = reef(&daemon);
        command.args(["-o", "json", "ws", "create", "--repo", &repo.id]);
        command.args(["--owner", "someone@elsewhere"]);
        command
    })
    .await;
    let created: Value = serde_json::from_str(&stdout(&created)).unwrap();
    let id = created["id"].as_str().unwrap();
    let mut command = reef(&daemon);
    command.args(["ws", "run", "--workspace", id, "--", "true"]);
    let refused = finish(command).await;
    assert_eq!(code(&refused), 5, "{}", stderr(&refused));
    assert!(
        stderr(&refused).contains("--owner someone@elsewhere"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(workspace(&daemon, id).await["state"], "active");
    let mut command = reef(&daemon);
    command.args([
        "ws",
        "run",
        "--workspace",
        id,
        "--owner",
        "someone@elsewhere",
    ]);
    command.args(["--", "true"]);
    let output = finish(command).await;
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(workspace(&daemon, id).await["state"], "released");
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn daemon_down_mid_run() {
    let (daemon, repo) = with_short_leases().await;
    let child = spawn(ws_run(&daemon, &repo, &["--", "sleep", "4"]));
    wait_for(RECLAIM_BOUND, "workspace created", || async {
        !workspaces(&daemon).await.is_empty()
    })
    .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    daemon.stop().await;
    let output = wait(child).await;
    let text = stderr(&output);
    assert!(text.contains("reef: daemon unreachable"), "{text}");
    assert_eq!(
        text.matches("reef: daemon unreachable").count(),
        1,
        "{text}"
    );
    // The command succeeded and the release failed: the release's code.
    assert_eq!(code(&output), 6, "{text}");
    assert!(text.contains("error: "), "{text}");
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn json_output() {
    let (daemon, repo) = with_repo().await;
    let output = finish(ws_run(
        &daemon,
        &repo,
        &["-o", "json", "--", "sh", "-c", "exit 2"],
    ))
    .await;
    assert_eq!(code(&output), 2, "{}", stderr(&output));
    let report: Value = serde_json::from_str(&stdout(&output))
        .unwrap_or_else(|e| panic!("{e}: {}", stdout(&output)));
    assert_eq!(report["exit_code"], 2, "{report}");
    assert_eq!(report["released"], true, "{report}");
    assert_eq!(report["snapshot_taken"], true, "{report}");
    assert!(report["error"].is_null(), "{report}");
    let ws = only_workspace(&daemon).await;
    assert_eq!(report["workspace_id"], ws["id"]);
    assert_eq!(report["path"], ws["path"]);
    assert_eq!(report["change_id"], ws["change_id"]);
}
