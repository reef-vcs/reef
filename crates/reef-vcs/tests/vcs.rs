//! R2.27: the VCS commands in the CLI (`status`, `log`, `show`, `diff`,
//! `describe`, `abandon`, `undo`, `op`, `ws snapshot`, `file`) against a
//! daemon. Tests are numbered as the brief numbers them.
//!
//! Each runs a [`TestDaemon`] in this process with the watcher's snapshots
//! held off, so only the commands snapshot, and the `reef` binary against
//! its socket with `--no-autostart`, from a chosen directory. Stdout is
//! piped, so JSON is the default output.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use assert_cmd::Command;
use assert_cmd::assert::OutputAssertExt as _;
use reef_api::v1::{self, CreateWorkspaceRequest, create_workspace_request};
use reef_daemon::testing::{SyntheticRepo, TestDaemon};
use reef_testing::synth::Preset;
use serde_json::Value;

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
    })
    .await
}

async fn create_at(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    start: Option<create_workspace_request::Start>,
) -> v1::CreateWorkspaceResponse {
    daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "test@host".to_owned(),
            start,
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
}

async fn create(daemon: &TestDaemon, repo: &SyntheticRepo) -> v1::Workspace {
    create_at(daemon, repo, None).await.workspace.unwrap()
}

/// A workspace on a new change on `ws`'s change.
async fn create_on(daemon: &TestDaemon, repo: &SyntheticRepo, ws: &v1::Workspace) -> v1::Workspace {
    let start = create_workspace_request::Start::NewOn(ws.change_id.clone());
    create_at(daemon, repo, Some(start))
        .await
        .workspace
        .unwrap()
}

/// A second workspace editing `ws`'s change.
async fn create_sharing(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    ws: &v1::Workspace,
) -> v1::Workspace {
    let start = create_workspace_request::Start::Change(ws.change_id.clone());
    create_at(daemon, repo, Some(start))
        .await
        .workspace
        .unwrap()
}

fn path(ws: &v1::Workspace) -> PathBuf {
    PathBuf::from(&ws.path)
}

fn write(ws: &v1::Workspace, file: &str, contents: impl AsRef<[u8]>) {
    let file = path(ws).join(file);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, contents).unwrap();
}

/// `reef --no-autostart` against `daemon`, from `dir`, with a clean
/// environment.
fn reef(daemon: &TestDaemon, dir: &Path) -> Command {
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
    command
        .env("REEF_STATE_DIR", daemon.state_dir())
        .current_dir(dir)
        .arg("--no-autostart");
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

async fn run_in(daemon: &TestDaemon, dir: &Path, args: &[&str]) -> Output {
    run(reef(daemon, dir), args).await
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Stdout as one JSON value, after asserting success.
fn json(output: &Output) -> Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_str(&stdout(output)).unwrap_or_else(|e| panic!("{e}: {}", stdout(output)))
}

async fn ok(daemon: &TestDaemon, dir: &Path, args: &[&str]) -> Value {
    json(&run_in(daemon, dir, args).await)
}

/// The JSON error object, after asserting the exit code and `reason`.
fn failed(output: &Output, code: i32, reason: &str) -> Value {
    output.clone().assert().code(code);
    let value: Value = serde_json::from_str(&stdout(output))
        .unwrap_or_else(|e| panic!("{e}: {:?} {}", stdout(output), stderr(output)));
    assert_eq!(value["error"]["reason"], reason, "{value}");
    value["error"].clone()
}

async fn err(daemon: &TestDaemon, dir: &Path, args: &[&str], code: i32, reason: &str) -> Value {
    failed(&run_in(daemon, dir, args).await, code, reason)
}

fn next_of(error: &Value) -> Vec<String> {
    serde_json::from_value(error["next"].clone()).unwrap_or_else(|e| panic!("{e}: {error}"))
}

/// Runs a `next` verbatim (its first word is `reef`) from `dir`; a
/// `--no-autostart` it repeats from the command that failed is not added
/// twice.
async fn run_next(daemon: &TestDaemon, dir: &Path, next: &[String]) -> Output {
    assert_eq!(next[0], "reef", "{next:?}");
    let args: Vec<&str> = next[1..]
        .iter()
        .map(String::as_str)
        .filter(|arg| *arg != "--no-autostart")
        .collect();
    run_in(daemon, dir, &args).await
}

fn strings(value: &Value) -> Vec<String> {
    serde_json::from_value(value.clone()).unwrap()
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn scope_rules() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    write(&ws, "src/x.txt", "x\n");
    let in_status = |value: &Value| value["workspace_status"]["workspace"]["id"].clone();
    for dir in [path(&ws), path(&ws).join("src")] {
        let status = ok(&daemon, &dir, &["status"]).await;
        assert_eq!(in_status(&status), ws.id.as_str(), "{}", dir.display());
    }
    let mut from_env = reef(&daemon, &repo.root);
    from_env.env("REEF_WORKSPACE", &ws.id);
    let status = json(&run(from_env, &["status"]).await);
    assert_eq!(in_status(&status), ws.id.as_str());
    let inside = path(&ws).join("src");
    for flag in [&ws.id[..6], inside.to_str().unwrap()] {
        let status = ok(&daemon, &repo.root, &["status", "--workspace", flag]).await;
        assert_eq!(in_status(&status), ws.id.as_str(), "{flag}");
    }
    // `--repo <a workspace path>` narrowing `--workspace` names the
    // workspace's repository (review finding 5).
    let ws_path = path(&ws);
    let status = ok(
        &daemon,
        &repo.root,
        &[
            "status",
            "--workspace",
            &ws.id,
            "--repo",
            ws_path.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(in_status(&status), ws.id.as_str());
    // `--repo .` inside the workspace: its repository, and no workspace.
    let status = ok(&daemon, &path(&ws), &["status", "--repo", "."]).await;
    assert_eq!(status["workspace_status"], Value::Null);
    assert_eq!(status["repository"]["id"], repo.id.as_str());

    // A plain git checkout: the first status registers it.
    let plain = daemon.tmp.path().join("plain-git");
    std::fs::create_dir_all(&plain).unwrap();
    let plain = plain.canonicalize().unwrap();
    reef_testing::git::init_repo(&plain, &[("README", "plain git\n")]);
    let output = run_in(&daemon, &plain, &["--no-register", "status"]).await;
    let error = failed(&output, 3, "REPOSITORY_NOT_REGISTERED");
    assert_eq!(
        next_of(&error),
        ["reef", "repo", "register", plain.to_str().unwrap()]
    );
    let output = run_in(&daemon, &plain, &["status"]).await;
    assert!(
        stderr(&output).contains("reef: registered"),
        "{}",
        stderr(&output)
    );
    assert_eq!(json(&output)["workspace_status"], Value::Null);

    // A REEF_WORKSPACE nobody has: no fallback to the directory.
    let mut stale = reef(&daemon, &path(&ws));
    stale.env("REEF_WORKSPACE", "abcdabcdabcdabcd");
    failed(&run(stale, &["status"]).await, 3, "WORKSPACE_NOT_FOUND");
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn status_in_a_workspace_and_outside() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let existing = first_file(&path(&ws));
    write(&ws, &existing, "changed\n");
    write(&ws, "b-new.txt", "b\n");
    let status = ok(&daemon, &path(&ws), &["status"]).await;
    let ws_status = &status["workspace_status"];
    assert_eq!(
        ws_status["working_copy"]["change_id"],
        ws.change_id.as_str()
    );
    let files = ws_status["changed_files"].as_array().unwrap();
    let kind_of = |name: &str| {
        files
            .iter()
            .find(|file| file["path"] == name)
            .map(|file| file["kind"].clone())
    };
    assert_eq!(kind_of(&existing), Some("modified".into()), "{files:?}");
    assert_eq!(kind_of("b-new.txt"), Some("added".into()), "{files:?}");
    assert_eq!(ws_status["source"], "snapshot");
    assert_eq!(ws_status["snapshot"]["kind"], "snapshot");
    assert_eq!(ws_status["snapshot"]["snapshot_kind"], "explicit");

    write(&ws, "c-new.txt", "c\n");
    let status = ok(&daemon, &path(&ws), &["status", "--no-snapshot"]).await;
    assert_eq!(status["workspace_status"]["source"], "last-snapshot");
    assert_eq!(status["workspace_status"]["snapshot"], Value::Null);
    let status = ok(&daemon, &path(&ws), &["status", "--max-files", "1"]).await;
    let ws_status = &status["workspace_status"];
    assert_eq!(ws_status["changed_files"].as_array().unwrap().len(), 1);
    assert_eq!(ws_status["changed_file_count"], 3);

    let status = ok(&daemon, &repo.root, &["status"]).await;
    assert_eq!(status["workspace_status"], Value::Null);
    assert_eq!(status["repository_status"]["active_workspace_count"], 1);

    let output = run_in(&daemon, &path(&ws), &["-o", "text", "status"]).await;
    output.clone().assert().success();
    let text = stdout(&output);
    assert!(
        text.starts_with(&format!("Workspace {} on ", ws.id)),
        "{text}"
    );
    assert!(text.contains(&format!("\nM {existing}\n")), "{text}");
}

/// The first regular file at `root`, by name, not hidden.
fn first_file(root: &Path) -> String {
    let mut names: Vec<String> = std::fs::read_dir(root)
        .unwrap()
        .map(|entry| entry.unwrap())
        .filter(|entry| entry.file_type().unwrap().is_file())
        .map(|entry| entry.file_name().into_string().unwrap())
        .filter(|name| !name.starts_with('.'))
        .collect();
    names.sort();
    names.into_iter().next().unwrap()
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn status_is_a_command() {
    let (daemon, repo) = fixture().await;
    let status = ok(&daemon, &repo.root, &["status"]).await;
    assert_eq!(status["repository"]["id"], repo.id.as_str());
    for guess in ["info", "whoami"] {
        let output = run_in(&daemon, &repo.root, &[guess]).await;
        output.clone().assert().code(2);
        assert!(stderr(&output).contains("\nnext: reef here\n"), "{guess}");
    }
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn log_lists_and_limits() {
    let (daemon, repo) = fixture().await;
    let before = ok(&daemon, &repo.root, &["op", "log", "--limit", "1"]).await;
    let before = before[0]["id"].as_str().unwrap().to_owned();
    let a = create(&daemon, &repo).await;
    write(&a, "a.txt", "a\n");
    let b = create_on(&daemon, &repo, &a).await;
    let c = create_on(&daemon, &repo, &b).await;

    let log = ok(&daemon, &path(&a), &["log"]).await;
    let entries = log.as_array().unwrap();
    let mine = entries
        .iter()
        .find(|entry| entry["change_id"] == a.change_id.as_str())
        .unwrap();
    assert!(mine["edges"].is_array(), "{mine}");
    let output = run_in(&daemon, &path(&a), &["-o", "text", "log"]).await;
    let text = stdout(&output);
    let short: String = mine["change_id_reverse_hex"]
        .as_str()
        .unwrap()
        .chars()
        .take(12)
        .collect();
    assert!(text.contains(&format!("@ {short} ")), "{text}");

    let trunk = ok(&daemon, &path(&a), &["log", "-r", "trunk()"]).await;
    assert_eq!(trunk.as_array().unwrap().len(), 1);

    let output = run_in(&daemon, &path(&c), &["log", "--limit", "1"]).await;
    assert_eq!(json(&output).as_array().unwrap().len(), 1);
    assert!(
        stderr(&output).contains("--limit 0 shows all"),
        "{}",
        stderr(&output)
    );
    let all = ok(&daemon, &path(&c), &["log", "--limit", "0"]).await;
    let ids: Vec<&str> = all
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["change_id"].as_str().unwrap())
        .collect();
    for ws in [&a, &b, &c] {
        assert!(ids.contains(&ws.change_id.as_str()), "{ids:?}");
    }

    let error = err(
        &daemon,
        &repo.root,
        &["log", "-r", "@"],
        2,
        "WORKSPACE_REQUIRED",
    )
    .await;
    assert!(
        error["hint"].as_str().unwrap().contains("--workspace"),
        "{error}"
    );
    assert!(error.get("next").is_none(), "{error}");

    let old = ok(
        &daemon,
        &repo.root,
        &["log", "--at-op", &before, "-r", "all()", "--limit", "0"],
    )
    .await;
    assert!(
        old.as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["change_id"] != a.change_id.as_str()),
        "{old}"
    );
}

/// Workspaces A and B editing one change, both with a snapshotted edit:
/// the change is divergent.
async fn diverge(daemon: &TestDaemon, repo: &SyntheticRepo) -> (v1::Workspace, v1::Workspace) {
    let a = create(daemon, repo).await;
    let b = create_sharing(daemon, repo, &a).await;
    write(&a, "from-a.txt", "a\n");
    write(&b, "from-b.txt", "b\n");
    for ws in [&a, &b] {
        assert!(
            daemon
                .client
                .snapshot_workspace(&repo.id, &ws.id, "")
                .await
                .unwrap()
                .changed
        );
    }
    (a, b)
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn show_and_ambiguity() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    write(&ws, "shown.txt", "shown\n");
    let shown = ok(&daemon, &path(&ws), &["show"]).await;
    assert_eq!(shown["change_id"], ws.change_id.as_str());
    assert!(shown["at_operation"].is_string());
    assert_eq!(shown["total_files"], 1);
    assert_eq!(shown["files"][0]["path"], "shown.txt");
    let commit = shown["commit_id"].as_str().unwrap();
    let again = ok(&daemon, &repo.root, &["show", commit]).await;
    assert_eq!(again["commit_id"], commit);

    let (a, b) = diverge(&daemon, &repo).await;
    let error = err(
        &daemon,
        &path(&a),
        &["show", &a.change_id],
        2,
        "AMBIGUOUS_REVISION",
    )
    .await;
    let listed = error["metadata"]["commit_ids"].as_str().unwrap();
    assert_eq!(listed.split(',').count(), 2, "{error}");
    let next = next_of(&error);
    let every = listed.replace(',', "|");
    assert_eq!(
        next,
        [
            "reef",
            "log",
            "-r",
            every.as_str(),
            "--workspace",
            a.id.as_str()
        ]
    );
    let both = json(&run_next(&daemon, &path(&b), &next).await);
    assert_eq!(both.as_array().unwrap().len(), 2, "{both}");
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn diff_forms_and_paths() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    write(&ws, "a", "one\n");
    write(&ws, "src/a.rs", "fn a() {}\n");
    write(&ws, "bin.dat", [0_u8, 159, 146, 150, 0, 1]);
    let output = run_in(&daemon, &path(&ws), &["-o", "text", "diff", "a"]).await;
    let text = stdout(&output);
    assert!(text.starts_with("diff --git a/a b/a\n"), "{text}");
    assert!(text.contains("@@ -0,0 +1,1 @@\n+one\n"), "{text}");
    let output = run_in(&daemon, &path(&ws), &["-o", "text", "diff", "--stat"]).await;
    assert!(
        stdout(&output).contains("a | +1 -0\n"),
        "{}",
        stdout(&output)
    );
    let output = run_in(&daemon, &path(&ws), &["-o", "text", "diff", "--summary"]).await;
    assert!(
        stdout(&output).contains("A src/a.rs\n"),
        "{}",
        stdout(&output)
    );

    let diff = ok(&daemon, &path(&ws), &["diff"]).await;
    for key in [
        "from_commit_ids",
        "to_commit_id",
        "at_operation",
        "total_files",
    ] {
        assert!(diff.get(key).is_some(), "{key}: {diff}");
    }
    let file_a = diff["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "a")
        .unwrap();
    assert_eq!(file_a["hunks"].as_array().unwrap().len(), 1);
    let binary = diff["files"]
        .as_array()
        .unwrap()
        .iter()
        .find(|file| file["path"] == "bin.dat")
        .unwrap();
    assert_eq!(binary["binary"], true);
    let summary = ok(&daemon, &path(&ws), &["diff", "--summary"]).await;
    assert_eq!(summary["files"][0]["hunks"], serde_json::json!([]));
    let from = ok(&daemon, &path(&ws), &["diff", "--from", "@-"]).await;
    assert_eq!(from["total_files"], diff["total_files"]);

    // From a subdirectory, paths are relative to it; printed paths are not.
    let src = path(&ws).join("src");
    let only = ok(&daemon, &src, &["diff", "a.rs"]).await;
    assert_eq!(only["total_files"], 1);
    assert_eq!(only["files"][0]["path"], "src/a.rs");
    let error = err(&daemon, &src, &["diff", "../../x"], 2, "INVALID_PATH").await;
    assert!(
        error["message"].as_str().unwrap().contains(" is outside "),
        "{error}"
    );

    // A rename of identical content.
    daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let moved = ok(&daemon, &path(&ws), &["describe", "-m", "base"]).await;
    assert!(moved["operation"].is_object());
    let child = create_on(&daemon, &repo, &ws).await;
    std::fs::rename(path(&child).join("a"), path(&child).join("renamed")).unwrap();
    let renamed = ok(&daemon, &path(&child), &["diff", "--summary"]).await;
    assert_eq!(renamed["files"][0]["kind"], "renamed", "{renamed}");
    assert_eq!(renamed["files"][0]["old_path"], "a");
    let output = run_in(&daemon, &path(&child), &["-o", "text", "diff"]).await;
    assert!(
        stdout(&output).contains("rename from a\nrename to renamed\n"),
        "{}",
        stdout(&output)
    );
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn describe() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    let described = ok(&daemon, &dir, &["describe", "-m", "fix parser"]).await;
    assert_eq!(described["change"]["description"], "fix parser\n");
    assert_eq!(described["operation"]["kind"], "describe");
    assert_eq!(described["workspace"]["id"], ws.id.as_str());
    let two = ok(&daemon, &dir, &["describe", "-m", "title", "-m", "body"]).await;
    assert_eq!(two["change"]["description"], "title\n\nbody\n");
    let same = ok(&daemon, &dir, &["describe", "-m", "title", "-m", "body"]).await;
    assert_eq!(same["operation"], Value::Null);

    let output = run_in(&daemon, &dir, &["describe"]).await;
    output.clone().assert().code(2);
    assert!(stderr(&output).contains("--message"), "{}", stderr(&output));
    let error = err(
        &daemon,
        &dir,
        &["describe", "root()", "-m", "x"],
        5,
        "IMMUTABLE_REVISION",
    )
    .await;
    assert!(
        error["hint"].as_str().unwrap().contains("immutable"),
        "{error}"
    );

    let error = err(
        &daemon,
        &dir,
        &["describe", "-m", "x", "--expected-commit", &ws.commit_id],
        5,
        "EXPECTED_COMMIT_MISMATCH",
    )
    .await;
    let current = error["metadata"]["commit_id"].as_str().unwrap();
    let next = next_of(&error);
    assert_eq!(
        next,
        ["reef", "show", current, "--workspace", ws.id.as_str()]
    );
    json(&run_next(&daemon, &repo.root, &next).await);
    let error = err(
        &daemon,
        &dir,
        &["describe", "-m", "x", "--expected-commit", "abc"],
        2,
        "INVALID_COMMIT_ID",
    )
    .await;
    assert_eq!(
        next_of(&error),
        ["reef", "show", "@", "--workspace", ws.id.as_str()]
    );
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn abandon() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let child = create_on(&daemon, &repo, &ws).await;
    write(&child, "child.txt", "child\n");
    daemon
        .client
        .snapshot_workspace(&repo.id, &child.id, "")
        .await
        .unwrap();
    let abandoned = ok(&daemon, &path(&ws), &["abandon", &child.change_id]).await;
    assert!(abandoned["abandoned_commit_id"].is_string());
    assert_eq!(abandoned["operation"]["kind"], "abandon");
    assert_eq!(abandoned["workspace"]["id"], ws.id.as_str());
    let output = run_in(&daemon, &path(&ws), &["abandon"]).await;
    output.clone().assert().code(2);
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn undo() {
    let (daemon, repo) = fixture().await;
    let created = create_at(&daemon, &repo, None).await;
    let ws = created.workspace.unwrap();
    let dir = path(&ws);
    let described = ok(&daemon, &dir, &["describe", "-m", "first"]).await;
    let op = described["operation"]["id"].as_str().unwrap().to_owned();
    let undone = ok(&daemon, &dir, &["undo", &op]).await;
    assert_eq!(undone["undone_operation_id"], op.as_str());
    assert_eq!(undone["workspace"]["id"], ws.id.as_str());
    let shown = ok(&daemon, &dir, &["show"]).await;
    assert_eq!(shown["description"], "");

    let output = run_in(&daemon, &dir, &["undo"]).await;
    output.clone().assert().code(2);
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    let next = next_of(&value["error"]);
    assert_eq!(next, ["reef", "op", "log", "--repo", repo.id.as_str()]);
    json(&run_next(&daemon, &dir, &next).await);

    let create_op = created.operation.unwrap().id;
    err(
        &daemon,
        &dir,
        &["undo", &create_op],
        2,
        "OPERATION_NOT_UNDOABLE",
    )
    .await;

    let first = ok(&daemon, &dir, &["describe", "-m", "one"]).await;
    ok(&daemon, &dir, &["describe", "-m", "two"]).await;
    let first = first["operation"]["id"].as_str().unwrap();
    let error = err(&daemon, &dir, &["undo", first], 5, "OPERATION_SUPERSEDED").await;
    assert_eq!(
        next_of(&error),
        ["reef", "op", "log", "--repo", repo.id.as_str()]
    );
    let error = err(
        &daemon,
        &dir,
        &["undo", "abcdef0123"],
        3,
        "OPERATION_NOT_FOUND",
    )
    .await;
    assert_eq!(
        next_of(&error),
        ["reef", "op", "log", "--repo", repo.id.as_str()]
    );
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn op_log_and_show() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    write(&ws, "one.txt", "1\n");
    ok(&daemon, &dir, &["ws", "snapshot"]).await;
    write(&ws, "two.txt", "2\n");
    ok(&daemon, &dir, &["ws", "snapshot"]).await;
    let described = ok(&daemon, &dir, &["--agent", "bot-1", "describe", "-m", "x"]).await;
    let op = described["operation"]["id"].as_str().unwrap().to_owned();
    ok(&daemon, &dir, &["undo", &op]).await;

    let log = ok(&daemon, &dir, &["op", "log"]).await;
    let ops = log.as_array().unwrap();
    assert_eq!(ops[0]["kind"], "undo", "{log}");
    assert_eq!(ops[1]["id"], op.as_str());
    assert!(ops.iter().all(|op| op["effects"].is_null()));
    let ended: Vec<&str> = ops
        .iter()
        .map(|op| op["ended_at"].as_str().unwrap())
        .collect();
    assert!(ended.windows(2).all(|pair| pair[0] >= pair[1]), "{ended:?}");

    let kinds = |value: &Value| -> Vec<String> {
        value
            .as_array()
            .unwrap()
            .iter()
            .map(|op| op["kind"].as_str().unwrap().to_owned())
            .collect()
    };
    let describe_ops = ok(&daemon, &dir, &["op", "log", "--kind", "describe"]).await;
    assert_eq!(kinds(&describe_ops), ["describe"]);
    let snapshots = ok(&daemon, &dir, &["op", "log", "--kind", "snapshot"]).await;
    assert!(kinds(&snapshots).iter().all(|kind| kind == "snapshot"));
    assert!(kinds(&snapshots).len() >= 2);
    let by = ok(&daemon, &dir, &["op", "log", "--by", "bot-1"]).await;
    assert_eq!(by.as_array().unwrap().len(), 1);
    for flag in [&ws.id[..6], dir.to_str().unwrap()] {
        let mine = ok(&daemon, &repo.root, &["op", "log", "--workspace", flag]).await;
        assert!(
            mine.as_array()
                .unwrap()
                .iter()
                .all(|op| op["workspace_id"] == ws.id.as_str()),
            "{flag}: {mine}"
        );
    }
    let rh = described["change"]["change_id_reverse_hex"]
        .as_str()
        .unwrap();
    let touched = ok(&daemon, &dir, &["op", "log", "--change", &rh[..8]]).await;
    assert_ne!(touched.as_array().unwrap().len(), 0);
    let output = run_in(&daemon, &dir, &["op", "log", "--kind", "nope"]).await;
    output.clone().assert().code(2);
    assert!(
        stderr(&output).contains("create-workspace"),
        "{}",
        stderr(&output)
    );
    err(
        &daemon,
        &dir,
        &["op", "log", "--change", "zzzz!"],
        2,
        "INVALID_CHANGE_ID",
    )
    .await;
    let output = run_in(&daemon, &dir, &["op", "log", "--limit", "2"]).await;
    assert_eq!(json(&output).as_array().unwrap().len(), 2);
    assert!(stderr(&output).contains("--limit 0 shows all"));
    let effects = ok(&daemon, &dir, &["op", "log", "--effects", "--limit", "1"]).await;
    assert!(effects[0]["effects"].is_object(), "{effects}");

    let shown = ok(&daemon, &dir, &["op", "show", &op[..12]]).await;
    assert_eq!(shown["id"], op.as_str());
    assert!(shown["effects"].is_object(), "{shown}");
    let alias = ok(&daemon, &dir, &["operation", "show", &op[..12]]).await;
    assert_eq!(alias, shown);
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn ws_snapshot() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    write(&ws, "edit.txt", "edit\n");
    let taken = ok(&daemon, &path(&ws), &["ws", "snapshot", "-m", "checkpoint"]).await;
    assert_eq!(taken["changed"], true);
    assert_eq!(taken["operation"]["kind"], "snapshot");
    assert_eq!(taken["operation"]["snapshot_kind"], "explicit");
    assert_eq!(taken["operation"]["description"], "checkpoint");
    assert_eq!(taken["workspace"]["id"], ws.id.as_str());
    let again = ok(&daemon, &path(&ws), &["ws", "snapshot"]).await;
    assert_eq!(again["changed"], false);
    assert_eq!(again["operation"], Value::Null);
    let error = err(
        &daemon,
        &repo.root,
        &["ws", "snapshot"],
        2,
        "WORKSPACE_REQUIRED",
    )
    .await;
    assert_eq!(next_of(&error), ["reef", "ws", "list"]);
    ok(&daemon, &repo.root, &["ws", "snapshot", &ws.id]).await;
}

// 12
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one fixture, every read case")]
async fn file_read() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    write(&ws, "a", "first\n");
    // Tracked while small: jj leaves a new file over 1 MiB untracked.
    write(&ws, "big.bin", "small\n");
    let first = ok(&daemon, &path(&ws), &["ws", "snapshot"]).await;
    let first_commit = first["commit_id"].as_str().unwrap().to_owned();
    write(&ws, "a", "second\n");
    let big: Vec<u8> = (0..3 * 1024 * 1024 + 17)
        .map(|i: u32| u8::try_from(i % 251).unwrap())
        .collect();
    write(&ws, "big.bin", &big);
    write(&ws, "src/a.rs", "fn a() {}\n");

    let output = run_in(&daemon, &path(&ws), &["file", "read", "a"]).await;
    output.clone().assert().success();
    assert_eq!(output.stdout, b"second\n");
    let output = run_in(&daemon, &path(&ws), &["file", "read", "big.bin"]).await;
    output.clone().assert().success();
    assert!(output.stdout == big, "{} bytes", output.stdout.len());
    let output = run_in(
        &daemon,
        &path(&ws),
        &["file", "read", "a", "-r", &first_commit],
    )
    .await;
    assert_eq!(output.stdout, b"first\n");

    let text = ok(&daemon, &path(&ws), &["-o", "json", "file", "read", "a"]).await;
    assert_eq!(text["content"], "second\n");
    assert_eq!(text["content_base64"], Value::Null);
    let output = run_in(
        &daemon,
        &path(&ws),
        &["-o", "json", "file", "read", "big.bin"],
    )
    .await;
    let binary = json(&output);
    assert_eq!(binary["content"], Value::Null);
    assert!(binary["content_base64"].is_string());

    let src = path(&ws).join("src");
    let output = run_in(&daemon, &src, &["file", "read", "a.rs"]).await;
    assert_eq!(output.stdout, b"fn a() {}\n");
    // The root is a directory, as any other (review finding 4).
    let error = err(
        &daemon,
        &path(&ws),
        &["-o", "json", "file", "read", "."],
        5,
        "NOT_A_FILE",
    )
    .await;
    let next = next_of(&error);
    assert_eq!(
        next,
        [
            "reef",
            "file",
            "list",
            "-r",
            "@",
            "--workspace",
            ws.id.as_str()
        ]
    );
    json(&run_next(&daemon, &repo.root, &next).await);

    // `file read` prints text even on a pipe, so its errors are JSON only
    // when asked.
    let error = err(
        &daemon,
        &path(&ws),
        &["-o", "json", "file", "read", "src"],
        5,
        "NOT_A_FILE",
    )
    .await;
    let commit = error["metadata"]["commit_id"].as_str().unwrap().to_owned();
    let next = next_of(&error);
    assert_eq!(
        next,
        [
            "reef",
            "file",
            "list",
            "src",
            "-r",
            commit.as_str(),
            "--workspace",
            ws.id.as_str()
        ]
    );
    json(&run_next(&daemon, &repo.root, &next).await);
    let error = err(
        &daemon,
        &src,
        &["-o", "json", "file", "read", "nope.rs"],
        3,
        "PATH_NOT_FOUND",
    )
    .await;
    let next = next_of(&error);
    assert_eq!(
        next,
        [
            "reef",
            "file",
            "list",
            "src",
            "-r",
            commit.as_str(),
            "--workspace",
            ws.id.as_str()
        ]
    );
    json(&run_next(&daemon, &repo.root, &next).await);
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn file_list() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    write(&ws, "src/a.rs", "fn a() {}\n");
    write(&ws, "src/deep/b.rs", "fn b() {}\n");
    for i in 0..1200 {
        write(&ws, &format!("many/f{i:04}"), "x\n");
    }
    let root = ok(&daemon, &path(&ws), &["file", "list"]).await;
    let entries = root.as_array().unwrap();
    assert!(
        entries
            .iter()
            .any(|entry| entry["path"] == "src" && entry["type"] == "directory"),
        "{root}"
    );
    let output = run_in(&daemon, &path(&ws), &["-o", "text", "file", "list"]).await;
    assert!(stdout(&output).lines().any(|line| line == "src/"));

    let src = path(&ws).join("src");
    let here = ok(&daemon, &src, &["file", "list"]).await;
    let paths: Vec<&str> = here
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["src/a.rs", "src/deep"]);
    let deep = ok(&daemon, &src, &["file", "list", "--recursive"]).await;
    assert!(
        deep.as_array()
            .unwrap()
            .iter()
            .all(|entry| entry["type"] != "directory")
    );
    let many = ok(&daemon, &path(&ws), &["file", "ls", "many"]).await;
    assert_eq!(many.as_array().unwrap().len(), 1200);

    let error = err(
        &daemon,
        &src,
        &["file", "list", "a.rs"],
        5,
        "NOT_A_DIRECTORY",
    )
    .await;
    let commit = error["metadata"]["commit_id"].as_str().unwrap();
    let next = next_of(&error);
    assert_eq!(
        next,
        [
            "reef",
            "file",
            "read",
            "src/a.rs",
            "-r",
            commit,
            "--workspace",
            ws.id.as_str()
        ]
    );
    let output = run_next(&daemon, &repo.root, &next).await;
    assert_eq!(output.stdout, b"fn a() {}\n");
    let output = run_in(&daemon, &src, &["file", "cat", "a.rs"]).await;
    assert_eq!(output.stdout, b"fn a() {}\n");
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn agents_on_new_writes() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let child = create_on(&daemon, &repo, &ws).await;
    let dir = path(&ws);
    let agent = |value: &Value| value["operation"]["agent"].clone();
    let described = ok(&daemon, &dir, &["--agent", "bot-4", "describe", "-m", "x"]).await;
    assert_eq!(agent(&described), "bot-4");
    let op = described["operation"]["id"].as_str().unwrap().to_owned();
    let undone = ok(&daemon, &dir, &["--agent", "bot-4", "undo", &op]).await;
    assert_eq!(agent(&undone), "bot-4");
    let abandoned = ok(
        &daemon,
        &dir,
        &["--agent", "bot-4", "abandon", &child.change_id],
    )
    .await;
    assert_eq!(agent(&abandoned), "bot-4");
    write(&ws, "edit.txt", "edit\n");
    let taken = ok(&daemon, &dir, &["--agent", "bot-4", "ws", "snapshot"]).await;
    assert_eq!(agent(&taken), "bot-4");

    write(&ws, "edit2.txt", "edit\n");
    let mut command = reef(&daemon, &dir);
    command.env("REEF_AGENT", "bot-5");
    let status = json(&run(command, &["status"]).await);
    assert_eq!(status["workspace_status"]["snapshot"]["agent"], "bot-5");
}

// 15
#[tokio::test(flavor = "multi_thread")]
async fn error_metadata() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let error = err(
        &daemon,
        &path(&ws),
        &["describe", "-m", "x", "--expected-commit", &"0".repeat(40)],
        5,
        "EXPECTED_COMMIT_MISMATCH",
    )
    .await;
    assert!(
        error["metadata"]["expected_commit_id"].is_string(),
        "{error}"
    );
    assert!(error["metadata"]["commit_id"].is_string(), "{error}");

    let other = create(&daemon, &repo).await;
    ok(&daemon, &repo.root, &["ws", "release", &other.id]).await;
    let output = run_in(&daemon, &repo.root, &["ws", "release", &other.id]).await;
    output.clone().assert().code(5);
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert!(value["error"].get("metadata").is_none(), "{value}");

    let output = run_in(
        &daemon,
        &path(&ws),
        &[
            "-o",
            "text",
            "describe",
            "-m",
            "x",
            "--expected-commit",
            &"0".repeat(40),
        ],
    )
    .await;
    output.clone().assert().code(5);
    assert_eq!(stdout(&output), "");
    assert!(!stderr(&output).contains("metadata"), "{}", stderr(&output));
}

// 16
#[tokio::test(flavor = "multi_thread")]
async fn divergence_is_shown() {
    let (daemon, repo) = fixture().await;
    let a = create(&daemon, &repo).await;
    let b = create_sharing(&daemon, &repo, &a).await;
    write(&a, "from-a.txt", "a\n");
    write(&b, "from-b.txt", "b\n");
    let in_a = ok(&daemon, &path(&a), &["ws", "snapshot"]).await;
    let output = run_in(&daemon, &path(&b), &["ws", "snapshot"]).await;
    let in_b = json(&output);
    assert_eq!(in_b["workspace"]["divergent"], true, "{in_b}");
    let rh: String = in_b["workspace"]["change_id_reverse_hex"]
        .as_str()
        .unwrap()
        .to_owned();
    let log = format!("reef log --workspace {} -r 'change_id({rh})'", b.id);
    assert!(stderr(&output).contains(&log), "{}", stderr(&output));

    let here = ok(&daemon, &path(&b), &["here"]).await;
    let next: Vec<String> = strings(&here["next"]);
    assert_eq!(
        next,
        [
            "reef",
            "log",
            "--workspace",
            b.id.as_str(),
            "-r",
            format!("change_id({rh})").as_str()
        ]
    );
    let both = json(&run_next(&daemon, &repo.root, &next).await);
    assert_eq!(both.as_array().unwrap().len(), 2, "{both}");

    let a_commit = in_a["commit_id"].as_str().unwrap();
    ok(&daemon, &path(&b), &["abandon", a_commit]).await;
    let here = ok(&daemon, &path(&b), &["here"]).await;
    assert_eq!(here["divergent"], false, "{here}");
    assert_eq!(
        strings(&here["next"]),
        ["reef", "ws", "release", b.id.as_str()]
    );
    let status = ok(&daemon, &repo.root, &["status"]).await;
    assert_eq!(
        status["repository_status"]["divergent_change_ids"],
        serde_json::json!([])
    );
}

// 17
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_failure_offers_no_snapshot() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let blocked = path(&ws).join("blocked");
    std::fs::create_dir(&blocked).unwrap();
    std::fs::write(blocked.join("inside.txt"), "unreadable\n").unwrap();
    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    if std::fs::read_dir(&blocked).is_ok() {
        std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
        eprintln!("skipped: permissions are not enforced here (running as root?)");
        return;
    }
    // A write that snapshots first (a describe takes none, R2.03) keeps
    // the daemon's `next`.
    let output = run_in(&daemon, &path(&ws), &["abandon", "@"]).await;
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["error"]["reason"], "SNAPSHOT_FAILED", "{value}");
    assert_eq!(
        next_of(&value["error"]),
        ["reef", "ws", "inspect", ws.id.as_str()]
    );
    let output = run_in(&daemon, &path(&ws), &["status", "--max-files", "5"]).await;
    output.clone().assert().failure();
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    let error = &value["error"];
    assert_eq!(error["reason"], "SNAPSHOT_FAILED", "{value}");
    assert_eq!(error["metadata"]["workspace_id"], ws.id.as_str());
    let next = next_of(error);
    assert_eq!(
        next,
        [
            "reef",
            "--no-autostart",
            "status",
            "--max-files",
            "5",
            "--no-snapshot"
        ]
    );
    let read = json(&run_next(&daemon, &path(&ws), &next).await);
    assert_eq!(read["workspace_status"]["source"], "last-snapshot");

    std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// 19
#[tokio::test(flavor = "multi_thread")]
async fn prefixes() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    let status = ok(&daemon, &dir, &["sta"]).await;
    assert!(status["workspace_status"].is_object());
    let shown = ok(&daemon, &dir, &["sh"]).await;
    assert_eq!(shown["change_id"], ws.change_id.as_str());
    let diff = ok(&daemon, &dir, &["di"]).await;
    assert!(diff["files"].is_array());
    let described = ok(&daemon, &dir, &["de", "-m", "x"]).await;
    assert_eq!(described["operation"]["kind"], "describe");
    let stores = ok(&daemon, &dir, &["sto", "ls"]).await;
    assert!(stores.is_array());
    for (prefix, candidates) in [
        ("st", &["status", "store"][..]),
        ("s", &["show", "status", "store"]),
        ("d", &["daemon", "describe", "diff"]),
    ] {
        let output = run_in(&daemon, &dir, &[prefix]).await;
        output.clone().assert().code(2);
        let text = stderr(&output);
        for candidate in candidates {
            assert!(text.contains(candidate), "{prefix}: {candidate}: {text}");
        }
    }
    // `a` names `abandon` and `guide` (clap counts its `agent-guide`
    // alias): clap's error for a subcommand it cannot pick, not
    // `abandon`'s for its missing REV, which a guess would give.
    let output = run_in(&daemon, &dir, &["a"]).await;
    output.clone().assert().code(2);
    let text = stderr(&output);
    assert!(text.contains("unrecognized subcommand 'a'"), "{text}");
    assert!(!text.contains("<REV>"), "{text}");
}

/// The newest operation's id, from `op log`.
async fn head_op(daemon: &TestDaemon, dir: &Path) -> String {
    ok(daemon, dir, &["op", "log", "--limit", "1"]).await[0]["id"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// Stdout's JSON lines.
fn json_lines(output: &Output) -> Vec<Value> {
    assert!(output.status.success(), "{}", stderr(output));
    stdout(output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}")))
        .collect()
}

// R2.09 18: `op watch` prints operations as they are recorded and ends by
// itself. Each watch starts from `--after` the head before the test's
// write, so a write made before it has subscribed is still seen.
#[tokio::test(flavor = "multi_thread")]
async fn op_watch() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);

    let head = head_op(&daemon, &dir).await;
    let mut command = reef(&daemon, &dir);
    command.args([
        "op", "watch", "--after", &head, "--kind", "describe", "--count", "1", "-o", "json",
    ]);
    let watch = tokio::task::spawn_blocking(move || command.output().unwrap());
    ok(&daemon, &dir, &["describe", "-m", "watched"]).await;
    let lines = json_lines(&watch.await.unwrap());
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["kind"], "describe");
    assert!(lines[0]["effects"].is_null());

    let head = head_op(&daemon, &dir).await;
    ok(&daemon, &dir, &["describe", "-m", "text"]).await;
    let text = run_in(
        &daemon,
        &dir,
        &[
            "op", "watch", "--after", &head, "--count", "1", "-o", "text",
        ],
    )
    .await;
    assert!(text.status.success(), "{}", stderr(&text));
    assert!(stdout(&text).contains("describe"), "{}", stdout(&text));

    // Nothing new: exit 0, nothing on stdout, and where to continue.
    let head = head_op(&daemon, &dir).await;
    let quiet = run_in(
        &daemon,
        &dir,
        &["op", "watch", "--after", &head, "--for", "1"],
    )
    .await;
    assert!(quiet.status.success(), "{}", stderr(&quiet));
    assert_eq!(stdout(&quiet), "");
    let note = stderr(&quiet);
    let continues = note
        .split("reef op watch --after ")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().next())
        .unwrap_or_else(|| panic!("{note}"))
        .to_owned();
    ok(&daemon, &dir, &["describe", "-m", "later"]).await;
    let later = json_lines(
        &run_in(
            &daemon,
            &dir,
            &["op", "watch", "--after", &continues, "--for", "0"],
        )
        .await,
    );
    assert_eq!(later.len(), 1, "{later:?}");
    assert_eq!(later[0]["kind"], "describe", "{later:?}");
    let with_effects = json_lines(
        &run_in(
            &daemon,
            &dir,
            &[
                "op",
                "watch",
                "--after",
                &continues,
                "--for",
                "0",
                "--effects",
            ],
        )
        .await,
    );
    assert!(with_effects[0]["effects"].is_object(), "{with_effects:?}");

    let usage = run_in(&daemon, &dir, &["op", "watch", "--kind", "nope"]).await;
    usage.clone().assert().code(2);
    assert!(stderr(&usage).contains("describe"), "{}", stderr(&usage));
    let bad = run_in(&daemon, &dir, &["op", "watch", "--after", "zzzz"]).await;
    failed(&bad, 2, "INVALID_OPERATION_ID");

    // `--follow` ends when nobody reads any more.
    let head = head_op(&daemon, &dir).await;
    ok(&daemon, &dir, &["describe", "-m", "piped"]).await;
    let script = format!(
        "{} --no-autostart op watch --after {head} --follow -o json | head -1",
        env!("CARGO_BIN_EXE_reef")
    );
    let script = format!("set -o pipefail; {script}");
    let mut shell = std::process::Command::new("bash");
    shell
        .args(["-c", &script])
        .env("REEF_STATE_DIR", daemon.state_dir())
        .current_dir(&dir);
    let piped = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || shell.output().unwrap()),
    )
    .await
    .expect("the pipeline ends")
    .unwrap();
    assert!(piped.status.success());
    assert_eq!(stdout(&piped).lines().count(), 1, "{}", stdout(&piped));
}

// R2.09 19: `op watch --follow` resubscribes from its cursor after the
// daemon restarts, printing nothing twice and missing nothing.
#[tokio::test(flavor = "multi_thread")]
async fn op_watch_survives_a_restart() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    let head = head_op(&daemon, &dir).await;
    let mut watch = std::process::Command::new(env!("CARGO_BIN_EXE_reef"));
    watch
        .env_remove("REEF_SOCKET")
        .env("REEF_STATE_DIR", daemon.state_dir())
        .current_dir(&dir)
        .args([
            "--no-autostart",
            "op",
            "watch",
            "--after",
            &head,
            "--kind",
            "describe",
            "--count",
            "2",
            "--follow",
            "-o",
            "json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = watch.spawn().unwrap();
    let subscriptions = &daemon.handle.state().subscriptions;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while subscriptions.active() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "the watch never subscribed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let before = ok(&daemon, &dir, &["describe", "-m", "before"]).await;
    let daemon = daemon.restart().await;
    let after = ok(&daemon, &dir, &["describe", "-m", "after"]).await;
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .expect("the watch ends at its count")
    .unwrap();
    let lines = json_lines(&output);
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert_eq!(lines[0]["id"], before["operation"]["id"]);
    assert_eq!(lines[1]["id"], after["operation"]["id"]);
}
