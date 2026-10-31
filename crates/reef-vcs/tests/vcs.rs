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
    let status = ok(
        &daemon,
        &path(&ws),
        &["-o", "json", "status", "--repo", "."],
    )
    .await;
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
    let status = ok(&daemon, &path(&ws), &["-o", "json", "status"]).await;
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
    let status = ok(&daemon, &repo.root, &["-o", "json", "status"]).await;
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

    let diff = ok(&daemon, &path(&ws), &["-o", "json", "diff"]).await;
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
    // Brief (ADR 0045): a summary's files have no hunks, so no key.
    let summary = ok(&daemon, &path(&ws), &["diff", "--summary"]).await;
    assert!(summary["files"][0].get("hunks").is_none(), "{summary}");
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
    let status = json(&run(command, &["-o", "json", "status"]).await);
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
    let status = ok(&daemon, &repo.root, &["-o", "json", "status"]).await;
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

fn activity(agent: &str, text: &str) -> v1::ReportPresenceRequest {
    v1::ReportPresenceRequest {
        agent: agent.to_owned(),
        kind: v1::PresenceKind::Activity.into(),
        activity: text.to_owned(),
        ..v1::ReportPresenceRequest::default()
    }
}

/// Waits until the daemon serves `n` subscriptions.
async fn subscribed(daemon: &TestDaemon, n: usize) {
    let subscriptions = &daemon.handle.state().subscriptions;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while subscriptions.active() < n {
        assert!(
            std::time::Instant::now() < deadline,
            "the watch never subscribed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// R2.10 19: `presence watch` prints the current set and changes, ends by
// itself, and filters by kind and agent.
#[tokio::test(flavor = "multi_thread")]
async fn presence_watch() {
    let (daemon, repo) = fixture().await;
    let dir = repo.root.clone();
    let empty = run_in(
        &daemon,
        &dir,
        &["presence", "watch", "--for", "0", "-o", "json"],
    )
    .await;
    assert!(empty.status.success(), "{}", stderr(&empty));
    assert_eq!(stdout(&empty), "");

    daemon
        .client
        .report_presence(activity("bot-1", "running tests"))
        .await
        .unwrap();
    let lines = json_lines(
        &run_in(
            &daemon,
            &dir,
            &["presence", "watch", "--for", "0", "-o", "json"],
        )
        .await,
    );
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert_eq!(lines[0]["ended"], false);
    assert!(lines[0]["end_reason"].is_null());
    assert_eq!(lines[0]["presence"]["kind"], "activity");
    assert_eq!(lines[0]["presence"]["agent"], "bot-1");
    assert_eq!(lines[0]["presence"]["activity"], "running tests");
    assert!(lines[0]["presence"]["workspace_id"].is_null());

    let ws = create(&daemon, &repo).await;
    let lease = json_lines(
        &run_in(
            &daemon,
            &dir,
            &[
                "presence", "watch", "--kind", "lease", "--for", "0", "-o", "json",
            ],
        )
        .await,
    );
    assert_eq!(lease.len(), 1, "{lease:?}");
    assert_eq!(lease[0]["presence"]["workspace_id"], ws.id.as_str());
    assert_eq!(
        lease[0]["presence"]["lease"]["id"],
        ws.lease.as_ref().unwrap().id.as_str()
    );
    assert_eq!(lease[0]["presence"]["by_daemon"], true);

    let mut command = reef(&daemon, &dir);
    command.args([
        "presence", "watch", "--by", "bot-2", "--count", "1", "-o", "json",
    ]);
    let watch = tokio::task::spawn_blocking(move || command.output().unwrap());
    subscribed(&daemon, 1).await;
    daemon
        .client
        .report_presence(activity("bot-2", "later"))
        .await
        .unwrap();
    let later = json_lines(&watch.await.unwrap());
    assert_eq!(later.len(), 1, "{later:?}");
    assert_eq!(later[0]["presence"]["activity"], "later");

    let text = run_in(
        &daemon,
        &dir,
        &[
            "presence", "watch", "--by", "bot-1", "--for", "0", "-o", "text",
        ],
    )
    .await;
    assert!(text.status.success(), "{}", stderr(&text));
    assert_eq!(stdout(&text).trim(), "activity bot-1 - running tests");

    let usage = run_in(&daemon, &dir, &["presence", "watch", "--kind", "nope"]).await;
    usage.clone().assert().code(2);
    assert!(stderr(&usage).contains("editing"), "{}", stderr(&usage));

    // `--follow` ends when nobody reads any more.
    let script = format!(
        "set -o pipefail; {} --no-autostart presence watch --follow -o json | head -1",
        env!("CARGO_BIN_EXE_reef")
    );
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

// R2.10 20: `presence watch --follow` resubscribes after the daemon
// restarts and stays a correct stream of changes: what vanished ends.
#[tokio::test(flavor = "multi_thread")]
async fn presence_watch_survives_a_restart() {
    let (daemon, repo) = fixture().await;
    let dir = repo.root.clone();
    let before = daemon
        .client
        .report_presence(activity("bot", "before"))
        .await
        .unwrap()
        .presence
        .unwrap();
    let mut watch = std::process::Command::new(env!("CARGO_BIN_EXE_reef"));
    watch
        .env_remove("REEF_SOCKET")
        .env("REEF_STATE_DIR", daemon.state_dir())
        .current_dir(&dir)
        .args([
            "--no-autostart",
            "presence",
            "watch",
            "--kind",
            "activity",
            "--count",
            "3",
            "--follow",
            "-o",
            "json",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = watch.spawn().unwrap();
    subscribed(&daemon, 1).await;
    let daemon = daemon.restart().await;
    subscribed(&daemon, 1).await;
    let after = daemon
        .client
        .report_presence(activity("bot", "after"))
        .await
        .unwrap()
        .presence
        .unwrap();
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .expect("the watch ends at its count")
    .unwrap();
    let lines = json_lines(&output);
    assert_eq!(lines.len(), 3, "{lines:?}");
    assert_eq!(lines[0]["presence"]["id"], before.id.as_str());
    assert_eq!(lines[0]["ended"], false);
    assert_eq!(lines[1]["presence"]["id"], before.id.as_str());
    assert_eq!(lines[1]["ended"], true);
    assert!(lines[1]["end_reason"].is_null());
    assert_eq!(lines[2]["presence"]["id"], after.id.as_str());
    assert_eq!(lines[2]["ended"], false);
}

/// An RFC 3339 time from the JSON as Unix milliseconds.
fn millis(value: &Value) -> u128 {
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("not a time: {value}"));
    humantime::parse_rfc3339_weak(text.trim_end_matches('Z'))
        .unwrap_or_else(|e| panic!("{e}: {text}"))
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}

/// The live presence entries the daemon holds of `kind`, by `agent`.
async fn entries(daemon: &TestDaemon, dir: &Path, kind: &str, agent: &str) -> Vec<Value> {
    json_lines(
        &run_in(
            daemon,
            dir,
            &[
                "presence", "watch", "--kind", kind, "--by", agent, "--for", "0", "-o", "json",
            ],
        )
        .await,
    )
    .into_iter()
    .map(|line| line["presence"].clone())
    .collect()
}

// R2.11 9: `presence report` in each of its forms and scopes, and `clear`.
#[tokio::test(flavor = "multi_thread")]
async fn presence_report_and_clear() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    let entry = ok(
        &daemon,
        &dir,
        &["presence", "report", "running the tests", "-o", "json"],
    )
    .await;
    assert_eq!(entry["kind"], "activity");
    assert_eq!(entry["activity"], "running the tests");
    assert_eq!(entry["workspace_id"], ws.id.as_str());
    assert_eq!(entry["change_id"], ws.change_id.as_str());
    assert_eq!(entry["agent"], "test@host", "the workspace's owner");
    assert_eq!(entry["by_daemon"], false);
    let id = entry["id"].as_str().unwrap().to_owned();

    let text = run_in(&daemon, &dir, &["presence", "report", "x", "-o", "text"]).await;
    assert!(text.status.success(), "{}", stderr(&text));
    let printed = stdout(&text);
    let printed_id = printed.strip_suffix('\n').unwrap();
    assert!(
        printed_id.len() == 16 && printed_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "{printed:?}"
    );
    ok(&daemon, &dir, &["presence", "clear", printed_id]).await;

    let replaced = ok(
        &daemon,
        &dir,
        &["presence", "report", "--id", &id, "linting"],
    )
    .await;
    assert_eq!(replaced["id"], id.as_str());
    let live = entries(&daemon, &dir, "activity", "test@host").await;
    assert_eq!(live.len(), 1, "{live:?}");
    assert_eq!(live[0]["activity"], "linting");

    std::fs::create_dir_all(dir.join("sub")).unwrap();
    let editing = ok(
        &daemon,
        &dir.join("sub"),
        &["presence", "report", "--path", "a.txt"],
    )
    .await;
    assert_eq!(editing["kind"], "editing");
    assert_eq!(strings(&editing["paths"]), ["sub/a.txt"]);

    let progress = ok(
        &daemon,
        &dir,
        &[
            "presence", "report", "--phase", "build", "--done", "3", "--total", "10", "--unit",
            "files",
        ],
    )
    .await;
    assert_eq!(progress["kind"], "progress");
    assert_eq!(
        progress["progress"],
        serde_json::json!({"phase": "build", "done": 3, "total": 10, "unit": "files"})
    );

    let short = ok(
        &daemon,
        &dir,
        &["presence", "report", "--ttl", "5s", "brief"],
    )
    .await;
    assert_eq!(
        millis(&short["expires_at"]) - millis(&short["updated_at"]),
        5_000
    );

    let in_repo = ok(
        &daemon,
        &repo.root,
        &["presence", "report", "--repo", &repo.id, "planning"],
    )
    .await;
    assert!(in_repo["workspace_id"].is_null(), "{in_repo}");
    assert_eq!(in_repo["repo_id"], repo.id.as_str());

    // A watcher sees the clear.
    let progress_id = progress["id"].as_str().unwrap().to_owned();
    let mut command = reef(&daemon, &dir);
    command.args([
        "presence", "watch", "--kind", "progress", "--count", "2", "-o", "json",
    ]);
    let watch = tokio::task::spawn_blocking(move || command.output().unwrap());
    subscribed(&daemon, 1).await;
    let cleared = ok(&daemon, &dir, &["presence", "clear", &progress_id]).await;
    assert_eq!(cleared, serde_json::json!({"cleared": [progress_id]}));
    let events = json_lines(&watch.await.unwrap());
    assert_eq!(events[1]["ended"], true, "{events:?}");
    assert_eq!(events[1]["end_reason"], "cleared");
    ok(&daemon, &dir, &["presence", "clear", &progress_id]).await;
}

// R2.11 10: the forms a report takes are checked before any call.
#[tokio::test(flavor = "multi_thread")]
async fn presence_report_usage() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    for args in [
        &["presence", "report"][..],
        &["presence", "report", "x", "--path", "a"],
        &["presence", "report", "--total", "3"],
        &["presence", "report", "x", "--unit", "files"],
        &["presence", "report", "x", "--id", "a", "--id-file", "f"],
        &["presence", "clear"],
    ] {
        let output = run_in(&daemon, &dir, args).await;
        assert_eq!(
            output.status.code(),
            Some(2),
            "{args:?}: {}",
            stderr(&output)
        );
    }
    err(
        &daemon,
        &dir,
        &["presence", "clear", "xyz"],
        2,
        "INVALID_PRESENCE_ID",
    )
    .await;
    let lease = entries(&daemon, &dir, "lease", "test@host").await;
    let lease_id = lease[0]["id"].as_str().unwrap();
    err(
        &daemon,
        &dir,
        &["presence", "clear", lease_id],
        2,
        "DAEMON_PRESENCE",
    )
    .await;
}

// R2.11 11: `--id-file` keeps one entry across processes, and `--id` of
// an entry that ended reports a new one.
#[tokio::test(flavor = "multi_thread")]
async fn presence_report_replaces_by_file() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    let scratch = tempfile::tempdir().unwrap();
    let file = scratch.path().join("presence");
    let file_arg = file.to_str().unwrap();
    let first = ok(
        &daemon,
        &dir,
        &["presence", "report", "--id-file", file_arg, "a"],
    )
    .await;
    let id = first["id"].as_str().unwrap().to_owned();
    assert_eq!(std::fs::read_to_string(&file).unwrap(), format!("{id}\n"));
    let second = ok(
        &daemon,
        &dir,
        &["presence", "report", "--id-file", file_arg, "b"],
    )
    .await;
    assert_eq!(second["id"], id.as_str());

    ok(&daemon, &dir, &["presence", "clear", &id]).await;
    let output = run_in(&daemon, &dir, &["presence", "report", "--id", &id, "c"]).await;
    let anew = json(&output);
    let anew_id = anew["id"].as_str().unwrap();
    assert_ne!(anew_id, id);
    assert!(
        stderr(&output).contains(&format!(
            "presence entry {id} had ended; reported a new one, {anew_id}"
        )),
        "{}",
        stderr(&output)
    );
    ok(&daemon, &dir, &["presence", "clear", anew_id]).await;

    // The file still names the cleared entry.
    let renewed = ok(
        &daemon,
        &dir,
        &["presence", "report", "--id-file", file_arg, "d"],
    )
    .await;
    let renewed_id = renewed["id"].as_str().unwrap().to_owned();
    assert_ne!(renewed_id, id);
    assert_eq!(
        std::fs::read_to_string(&file).unwrap(),
        format!("{renewed_id}\n")
    );
    let cleared = ok(&daemon, &dir, &["presence", "clear", "--id-file", file_arg]).await;
    assert_eq!(cleared, serde_json::json!({"cleared": [renewed_id]}));
    assert!(!file.exists());
    let again = ok(&daemon, &dir, &["presence", "clear", "--id-file", file_arg]).await;
    assert_eq!(again, serde_json::json!({"cleared": []}));
    let left = entries(&daemon, &dir, "activity", "test@host").await;
    assert!(left.is_empty(), "{left:?}");
}

// R2.11 12: the cap per agent, with the CLI's hint. Entries last ten
// minutes, so none lapses however slowly the 32 reports run.
#[tokio::test(flavor = "multi_thread")]
async fn presence_report_cap_hint() {
    let (daemon, repo) = fixture().await;
    let dir = repo.root.clone();
    for n in 0..32 {
        ok(
            &daemon,
            &dir,
            &[
                "--agent",
                "capbot",
                "presence",
                "report",
                "--ttl",
                "10m",
                &format!("task {n}"),
            ],
        )
        .await;
    }
    let error = err(
        &daemon,
        &dir,
        &["--agent", "capbot", "presence", "report", "one more"],
        6,
        "TOO_MANY_PRESENCE_ENTRIES",
    )
    .await;
    let hint = error["hint"].as_str().unwrap();
    assert!(hint.contains("--id-file"), "{hint}");
}

// R2.11 13: `ws renew` by id and from inside, and its refusals.
#[tokio::test(flavor = "multi_thread")]
async fn ws_renew() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let lease = ws.lease.as_ref().unwrap();
    let before = lease.expires_at.unwrap();
    let before = u128::try_from(before.seconds).unwrap() * 1000
        + u128::try_from(before.nanos / 1_000_000).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    let renewed = ok(&daemon, &repo.root, &["ws", "renew", &ws.id, "-o", "json"]).await;
    assert_eq!(renewed["workspace_id"], ws.id.as_str());
    assert_eq!(renewed["lease"]["id"], lease.id.as_str());
    let after = millis(&renewed["lease"]["expires_at"]);
    assert!(after > before, "{after} > {before}");

    let watched = entries(&daemon, &repo.root, "lease", "test@host").await;
    assert_eq!(watched.len(), 1, "{watched:?}");
    assert_eq!(millis(&watched[0]["lease"]["expires_at"]), after);

    let inside = ok(&daemon, &path(&ws), &["ws", "renew"]).await;
    assert_eq!(inside["lease"]["id"], lease.id.as_str());
    let text = run_in(&daemon, &path(&ws), &["ws", "renew", "-o", "text"]).await;
    assert!(text.status.success(), "{}", stderr(&text));
    assert!(
        stderr(&text).starts_with(&format!("renewed lease {} of {}: ", lease.id, ws.id)),
        "{}",
        stderr(&text)
    );

    let wrong = run_in(
        &daemon,
        &repo.root,
        &["ws", "renew", &ws.id, "--lease-id", "0000000000000000"],
    )
    .await;
    assert_eq!(wrong.status.code(), Some(5), "{}", stderr(&wrong));
    let wrong: Value = serde_json::from_str(&stdout(&wrong)).unwrap();
    assert!(wrong["error"]["next"].is_null(), "{wrong}");
    err(
        &daemon,
        &repo.root,
        &["ws", "renew"],
        2,
        "WORKSPACE_REQUIRED",
    )
    .await;
    let unknown = run_in(&daemon, &repo.root, &["ws", "renew", "ffffffffffffffff"]).await;
    assert_eq!(unknown.status.code(), Some(3), "{}", stderr(&unknown));

    ok(&daemon, &repo.root, &["ws", "release", &ws.id]).await;
    let released = run_in(&daemon, &repo.root, &["ws", "renew", &ws.id]).await;
    released.clone().assert().code(5);
    let error: Value = serde_json::from_str(&stdout(&released)).unwrap();
    assert_eq!(
        next_of(&error["error"]),
        ["reef", "ws", "create", "--repo", &repo.id]
    );
}

/// A protobuf length-delimited field.
fn field(out: &mut Vec<u8>, number: u32, bytes: &[u8]) {
    varint(out, u64::from(number << 3 | 2));
    varint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

fn varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(u8::try_from(value & 0x7f).unwrap() | 0x80);
        value >>= 7;
    }
    out.push(u8::try_from(value).unwrap());
}

/// A SCIP index, encoded by hand: the tool, the project root, and for each
/// path one definition of `f` at line 0, columns 4 to 5.
fn scip_index(root: &str, paths: &[&str]) -> Vec<u8> {
    let mut tool = Vec::new();
    field(&mut tool, 1, b"scip-python");
    field(&mut tool, 2, b"0.6.0");
    let mut metadata = Vec::new();
    field(&mut metadata, 2, &tool);
    field(&mut metadata, 3, format!("file://{root}").as_bytes());
    let mut index = Vec::new();
    field(&mut index, 1, &metadata);
    for path in paths {
        let mut range = Vec::new();
        for n in [0, 4, 5] {
            varint(&mut range, n);
        }
        let mut occurrence = Vec::new();
        field(&mut occurrence, 1, &range);
        field(
            &mut occurrence,
            2,
            format!("scip-python python x 1 `{path}`/f().").as_bytes(),
        );
        varint(&mut occurrence, 3 << 3);
        varint(&mut occurrence, 1);
        let mut document = Vec::new();
        field(&mut document, 1, path.as_bytes());
        field(&mut document, 2, &occurrence);
        field(&mut index, 2, &document);
    }
    index
}

// 22 (R2.14)
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "the three commands and their exits")]
async fn symbols_commands() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    write(&ws, "py/a.py", "def f():\n    return g()\n");
    write(&ws, "py/b.py", "def f():\n    pass\n");
    let dir = path(&ws);

    // A file lists inline, with 1-based text and 0-based JSON.
    let listed = ok(&daemon, &dir, &["symbols", "list", "py/a.py"]).await;
    let names: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["f", "g"], "{listed}");
    assert_eq!(listed[0]["range"], serde_json::json!([0, 4, 0, 5]));
    assert_eq!(listed[0]["kind"], "function");
    assert_eq!(listed[0]["role"], "definition");
    assert_eq!(
        listed[1]["enclosing_symbol"],
        "reef-tags . . . py/`a.py`/f()."
    );
    let text = run_in(&daemon, &dir, &["-o", "text", "sym", "ls", "py/a.py"]).await;
    assert_eq!(
        stdout(&text).lines().next(),
        Some("py/a.py:1:5  function  definition  f")
    );

    // Not indexed yet: a name query says what to run.
    let error = err(
        &daemon,
        &dir,
        &["symbols", "list", "--name", "f"],
        5,
        "SYMBOLS_NOT_INDEXED",
    )
    .await;
    assert_eq!(next_of(&error)[..3], ["reef", "symbols", "index"]);

    // Index: exit 6 while indexing, 0 once complete.
    let quick = run_in(&daemon, &dir, &["symbols", "index", "--wait", "0s"]).await;
    if !quick.status.success() {
        let error = failed(&quick, 6, "SYMBOLS_NOT_INDEXED");
        assert_eq!(next_of(&error)[..3], ["reef", "symbols", "index"]);
    }
    let status = ok(&daemon, &dir, &["symbols", "index", "--wait", "5m"]).await;
    assert_eq!(status["state"], "complete", "{status}");
    assert!(status["languages"]["python"].as_u64().unwrap() >= 2);
    let defs = ok(
        &daemon,
        &dir,
        &["symbols", "list", "--name", "f", "--definitions"],
    )
    .await;
    assert_eq!(defs.as_array().unwrap().len(), 2, "{defs}");
    let funcs = ok(
        &daemon,
        &dir,
        &[
            "symbols", "list", "py", "--kind", "function", "--limit", "1",
        ],
    )
    .await;
    assert_eq!(funcs.as_array().unwrap().len(), 1);

    // Upload: a.py accepted, b.py's file changed on disk (rejected), c.py unreadable (skipped).
    let index_dir = tempfile::tempdir().unwrap();
    let root = index_dir.path().join("py");
    std::fs::create_dir(&root).unwrap();
    std::fs::copy(dir.join("py/a.py"), root.join("a.py")).unwrap();
    std::fs::write(root.join("b.py"), "def f():\n    changed()\n").unwrap();
    let file = index_dir.path().join("index.scip");
    std::fs::write(
        &file,
        scip_index(root.to_str().unwrap(), &["a.py", "b.py", "c.py"]),
    )
    .unwrap();
    let uploaded = ok(
        &daemon,
        &dir,
        &[
            "symbols",
            "upload",
            file.to_str().unwrap(),
            "--prefix",
            "py",
        ],
    )
    .await;
    assert_eq!(uploaded["accepted"], 1, "{uploaded}");
    assert_eq!(uploaded["rejected"][0]["path"], "py/b.py");
    assert_eq!(uploaded["rejected"][0]["reason"], "content_mismatch");
    assert_eq!(uploaded["skipped"][0]["path"], "py/c.py");
    assert_eq!(uploaded["skipped"][0]["reason"], "unreadable");
    let scip = ok(&daemon, &dir, &["symbols", "list", "py/a.py"]).await;
    assert_eq!(scip[0]["source"], "scip", "{scip}");

    // Nothing accepted: exit 2.
    std::fs::write(&file, scip_index(root.to_str().unwrap(), &["b.py"])).unwrap();
    let output = run_in(
        &daemon,
        &dir,
        &[
            "symbols",
            "upload",
            file.to_str().unwrap(),
            "--prefix",
            "py",
        ],
    )
    .await;
    failed(&output, 2, "INVALID_UPLOAD");
}

// 20 (R2.15)
#[tokio::test(flavor = "multi_thread")]
async fn diff_symbols_cli() {
    let (daemon, repo) = fixture().await;
    let base = create(&daemon, &repo).await;
    write(&base, "lib/parse.py", "def parse(x):\n    return 1\n");
    write(&base, "app/use.py", "def main():\n    return parse(1)\n");
    ok(&daemon, &path(&base), &["status"]).await;
    let ws = create_on(&daemon, &repo, &base).await;
    write(&ws, "lib/parse.py", "def parse(x):\n    return 2\n");
    let dir = path(&ws);
    ok(&daemon, &dir, &["symbols", "index", "--wait", "5m"]).await;

    let text = run_in(&daemon, &dir, &["diff", "--symbols", "-o", "text"]).await;
    assert!(text.status.success(), "{}", stderr(&text));
    assert_eq!(
        stdout(&text),
        "M lib/parse.py\n  ~ function parse()  +1 -1\n      1 call site: app/use.py:2 in main() (by name)\n1 file changed; symbols: 1 changed\n"
    );
    let stat = run_in(
        &daemon,
        &dir,
        &["diff", "--symbols", "--stat", "-o", "text"],
    )
    .await;
    assert!(
        stdout(&stat).starts_with("M lib/parse.py | +1 -1\n"),
        "{}",
        stdout(&stat)
    );

    let diff = ok(&daemon, &dir, &["diff", "--symbols"]).await;
    assert_eq!(diff["symbol_summary"]["changed"], 1, "{diff}");
    let file = &diff["files"][0];
    assert_eq!(file["symbols"]["coverage"], "covered", "{diff}");
    let change = &file["symbols"]["changes"][0];
    assert_eq!(change["kind"], "changed");
    assert_eq!(change["call_site_status"], "found");
    let site = change["call_sites"][0].as_object().unwrap();
    let mut keys: Vec<&str> = site.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(keys, ["caller", "path", "range", "source"], "{diff}");

    // Without the flag, the JSON is R2.27's.
    let plain = ok(&daemon, &dir, &["diff"]).await;
    assert!(plain.get("symbol_summary").is_none(), "{plain}");
    assert!(plain["files"][0].get("symbols").is_none(), "{plain}");

    let none = ok(&daemon, &dir, &["diff", "--symbols", "--call-sites", "0"]).await;
    let change = &none["files"][0]["symbols"]["changes"][0];
    assert_eq!(change["call_sites"], serde_json::json!([]));
    assert!(change["call_site_status"].is_null(), "{none}");
    let scip = ok(&daemon, &dir, &["diff", "--symbols", "--source", "scip"]).await;
    assert_eq!(scip["files"][0]["symbols"]["coverage"], "no_scip");

    let expanded = run_in(&daemon, &dir, &["diff", "--expand", "parse", "-o", "text"]).await;
    assert_eq!(
        stdout(&expanded),
        "diff --git a/lib/parse.py b/lib/parse.py\n--- a/lib/parse.py\n+++ b/lib/parse.py\n@@ -1,2 +1,2 @@\n def parse(x):\n-    return 1\n+    return 2\n"
    );
    let missing = run_in(&daemon, &dir, &["diff", "--expand", "nothing"]).await;
    assert!(missing.status.success());
    assert!(
        stderr(&missing).contains("reef: no changed symbol matches nothing"),
        "{}",
        stderr(&missing)
    );
    let quiet = run_in(&daemon, &dir, &["-q", "diff", "--expand", "nothing"]).await;
    assert!(!stderr(&quiet).contains("no changed symbol"));

    for args in [
        &["diff", "--expand", "x", "--stat"][..],
        &["diff", "--call-sites", "3"][..],
        &["diff", "--source", "tags"][..],
    ] {
        let usage = run_in(&daemon, &dir, args).await;
        assert_eq!(usage.status.code(), Some(2), "{args:?}: {}", stderr(&usage));
    }
}

/// R2.16 test 22: `reef history`'s JSON and text, its filters and time
/// forms, grouping, the limit, and paths from a subdirectory.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one fixture, every form of the command"
)]
async fn history() {
    let (daemon, repo) = fixture().await;
    daemon
        .handle
        .state()
        .history
        .set_throttle(Duration::ZERO, Duration::ZERO);
    let ws = create(&daemon, &repo).await;
    write(&ws, "hist/parser.py", "def parse():\n    return 1\n");
    ok(&daemon, &path(&ws), &["status", "-o", "json"]).await;
    let other = create(&daemon, &repo).await;
    write(&other, "hist/other.txt", "other\n");
    ok(&daemon, &path(&other), &["status", "-o", "json"]).await;

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let found = loop {
        let found = ok(&daemon, &repo.root, &["history", "hist", "-o", "json"]).await;
        let changes = found["changes"].as_array().unwrap();
        if changes.len() == 2 && changes.iter().all(|c| c["touches_state"] == "current") {
            break found;
        }
        assert!(std::time::Instant::now() < deadline, "{found}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let keys: Vec<&String> = found.as_object().unwrap().keys().collect();
    assert_eq!(keys, ["changes", "coverage", "groups", "groups_truncated"]);
    let first = &found["changes"][0];
    assert_eq!(first["change_id"], other.change_id, "newest first: {found}");
    assert_eq!(first["outcome"], "open");
    assert_eq!(
        first["matched_paths"],
        serde_json::json!(["hist/other.txt"])
    );
    assert_eq!(first["landed_at"], Value::Null);

    // A workspace by path, and `.` at the root is every path.
    let ws_path = path(&ws).display().to_string();
    let by_ws = ok(
        &daemon,
        &repo.root,
        &["history", "--workspace", &ws_path, "-o", "json"],
    )
    .await;
    let ids: Vec<&Value> = by_ws["changes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| &c["change_id"])
        .collect();
    assert_eq!(ids, [&Value::from(ws.change_id.clone())], "{by_ws}");
    let everything = ok(&daemon, &repo.root, &["history", ".", "-o", "json"]).await;
    assert!(
        everything["changes"].as_array().unwrap().len() >= 2,
        "{everything}"
    );

    // Paths from a subdirectory are relative to it.
    let sub = ok(
        &daemon,
        &path(&ws).join("hist"),
        &["history", "parser.py", "-o", "json"],
    )
    .await;
    assert_eq!(sub["changes"].as_array().unwrap().len(), 1, "{sub}");
    assert_eq!(sub["changes"][0]["change_id"], ws.change_id);

    // By symbol, and its text.
    let by_symbol = ok(
        &daemon,
        &repo.root,
        &["history", "--symbol", "parse", "-o", "json"],
    )
    .await;
    assert_eq!(
        by_symbol["changes"][0]["touches"][0]["change"], "added",
        "{by_symbol}"
    );
    let text = stdout(
        &run_in(
            &daemon,
            &repo.root,
            &["history", "--symbol", "parse", "-o", "text"],
        )
        .await,
    );
    assert!(
        text.contains("+ function parse()  hist/parser.py"),
        "{text}"
    );
    assert!(
        text.lines().last().unwrap().starts_with("1 change"),
        "{text}"
    );

    // Time forms.
    ok(
        &daemon,
        &repo.root,
        &["history", "--since", "7d", "-o", "json"],
    )
    .await;
    ok(
        &daemon,
        &repo.root,
        &["history", "--since", "2020-01-01T00:00:00Z", "-o", "json"],
    )
    .await;
    run_in(&daemon, &repo.root, &["history", "--since", "soon"])
        .await
        .assert()
        .code(2);

    // Grouping, and the limit's note.
    let groups = ok(
        &daemon,
        &repo.root,
        &["history", "hist", "--group-by", "outcome", "-o", "json"],
    )
    .await;
    assert_eq!(groups["changes"], serde_json::json!([]));
    assert_eq!(groups["groups"][0]["outcome"], "open", "{groups}");
    assert_eq!(groups["groups"][0]["count"], 2);
    let limited = run_in(
        &daemon,
        &repo.root,
        &["history", "hist", "--limit", "1", "-o", "json"],
    )
    .await;
    assert_eq!(json(&limited)["changes"].as_array().unwrap().len(), 1);
    assert!(
        stderr(&limited).contains("--limit 0 shows all"),
        "{}",
        stderr(&limited)
    );

    // The daemon's reasons come through with their advice.
    let error = err(
        &daemon,
        &repo.root,
        &["history", "--since", "1d", "--until", "7d", "-o", "json"],
        2,
        "INVALID_HISTORY_QUERY",
    )
    .await;
    assert_eq!(next_of(&error), ["reef", "history", "--help"]);
}

/// Whether `value` is a zero value a brief view leaves out (ADR 0045).
fn is_zero(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(set) => !set,
        Value::Number(n) => n.as_u64() == Some(0),
        Value::String(text) => text.is_empty(),
        Value::Array(values) => values.is_empty(),
        Value::Object(_) => false,
    }
}

/// Every key of `full` is in `brief` with the same value, or is left out
/// with a zero value, or is a side's type `file` (ADR 0045's rules).
fn assert_brief_of(brief: &Value, full: &Value, at: &str) {
    match (brief, full) {
        (Value::Object(brief), Value::Object(full)) => {
            for key in brief.keys() {
                assert!(
                    full.contains_key(key),
                    "{at}.{key} is not in the full document"
                );
            }
            for (key, value) in full {
                match brief.get(key) {
                    Some(kept) => assert_brief_of(kept, value, &format!("{at}.{key}")),
                    None => assert!(
                        is_zero(value)
                            || (["old_type", "new_type"].contains(&key.as_str())
                                && value == "file"),
                        "{at}.{key} = {value} was left out"
                    ),
                }
            }
        }
        (Value::Array(brief), Value::Array(full)) => {
            assert_eq!(brief.len(), full.len(), "{at}");
            for (index, (brief, full)) in brief.iter().zip(full).enumerate() {
                assert_brief_of(brief, full, &format!("{at}[{index}]"));
            }
        }
        _ => assert_eq!(brief, full, "{at}"),
    }
}

// #114 test 16
#[tokio::test(flavor = "multi_thread")]
async fn status_brief_and_full() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let existing = first_file(&path(&ws));
    write(&ws, &existing, "changed\n");
    write(&ws, "b-new.txt", "b\n");
    write(&ws, "c-new.txt", "c\n");
    let brief = ok(&daemon, &path(&ws), &["status"]).await;
    let ws_status = &brief["workspace_status"];
    assert_eq!(ws_status["changed_file_count"], 3, "{brief}");
    for file in ws_status["changed_files"].as_array().unwrap() {
        let keys: Vec<&String> = file.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["kind", "path"], "{file}");
    }
    assert!(ws_status["parents"][0]["change_id_reverse_hex"].is_string());
    assert_eq!(ws_status["workspace"]["id"], ws.id.as_str());
    assert!(brief["repository_status"]["active_workspace_count"].is_u64());
    let text = stdout(&run_in(&daemon, &path(&ws), &["status", "--no-snapshot"]).await);
    assert_eq!(text.lines().count(), 1, "one compact object: {text}");
    for key in [
        "heads",
        "last_operation",
        "at_operation",
        "repository",
        "snapshot",
    ] {
        assert!(!text.contains(&format!("\"{key}\":")), "{key}: {text}");
    }
    let full = ok(
        &daemon,
        &path(&ws),
        &["-o", "json", "status", "--no-snapshot"],
    )
    .await;
    for key in ["repository", "at_operation"] {
        assert!(full.get(key).is_some(), "{key}: {full}");
    }
    for key in ["heads", "last_operation", "active_workspaces"] {
        assert!(
            full["repository_status"].get(key).is_some(),
            "{key}: {full}"
        );
    }
    let repository = ok(&daemon, &repo.root, &["status", "--repo", &repo.id]).await;
    assert!(repository.get("workspace_status").is_none(), "{repository}");
}

// #114 test 17
#[tokio::test(flavor = "multi_thread")]
async fn diff_brief_matches_full() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let existing = first_file(&path(&ws));
    write(&ws, "a", "one\n");
    daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    ok(&daemon, &path(&ws), &["describe", "-m", "base"]).await;
    let child = create_on(&daemon, &repo, &ws).await;
    write(&child, &existing, "changed\n");
    write(&child, "added.txt", "new\nno newline");
    std::fs::rename(path(&child).join("a"), path(&child).join("renamed")).unwrap();
    let dir = path(&child);
    for args in [&["diff", "--summary"][..], &["diff"]] {
        let brief = ok(&daemon, &dir, args).await;
        let full_args: Vec<&str> = ["-o", "json"].iter().chain(args).copied().collect();
        let mut full = ok(&daemon, &dir, &full_args).await;
        assert_eq!(brief["total_files"], 3, "{brief}");
        full.as_object_mut().unwrap().remove("at_operation");
        assert_brief_of(&brief, &full, "diff");
    }
}

// R2.12 13: `--at-op` and `--at` on status, `ws list`, `ws inspect` and
// `op log`.
#[tokio::test(flavor = "multi_thread")]
async fn replay_cli() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    write(&ws, "one.txt", "1\n");
    ok(&daemon, &dir, &["ws", "snapshot"]).await;
    let then = ok(&daemon, &dir, &["ws", "inspect", "--no-snapshot", &ws.id]).await;
    let x = head_op(&daemon, &dir).await;
    create(&daemon, &repo).await;
    write(&ws, "two.txt", "2\n");
    ok(&daemon, &dir, &["ws", "snapshot"]).await;

    let status = ok(
        &daemon,
        &dir,
        &["-o", "json", "status", "--at-op", &x[..12]],
    )
    .await;
    assert_eq!(status["at_operation"], x.as_str());
    assert_eq!(status["workspace_status"]["source"], "operation");
    assert_eq!(
        status["workspace_status"]["working_copy"]["commit_id"],
        then["commit_id"]
    );
    assert_eq!(status["repository_status"]["active_workspace_count"], 1);

    let listed = ok(&daemon, &dir, &["-o", "json", "ws", "list", "--at-op", &x]).await;
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|ws| ws["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, [ws.id.as_str()]);
    assert_eq!(
        ok(&daemon, &dir, &["ws", "list"])
            .await
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let inspected = ok(&daemon, &dir, &["ws", "inspect", &ws.id, "--at-op", &x]).await;
    assert_eq!(inspected["commit_id"], then["commit_id"]);
    let ops = ok(&daemon, &dir, &["op", "log", "--at-op", &x]).await;
    assert_eq!(ops[0]["id"], x.as_str());

    // `--at` resolves a time to the operation then, noted on stderr.
    let now = ok(&daemon, &dir, &["-o", "json", "status", "--at", "0s"]).await;
    assert_eq!(now["at_operation"], head_op(&daemon, &dir).await.as_str());
    let output = run_in(&daemon, &dir, &["ws", "list", "--at", "1s"]).await;
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(
        stderr(&output).starts_with("reef: at operation "),
        "{}",
        stderr(&output)
    );
    let rfc3339 = humantime::format_rfc3339(std::time::SystemTime::now()).to_string();
    ok(&daemon, &dir, &["op", "log", "--at", &rfc3339]).await;

    // Usage errors.
    for args in [
        &["status", "--at", "soon"][..],
        &["status", "--at", "1m", "--at-op", &x],
        &["ws", "list", "--all", "--at-op", &x],
    ] {
        run_in(&daemon, &dir, args).await.assert().code(2);
    }
    // Before the repository's first operation.
    let error = err(
        &daemon,
        &dir,
        &["status", "--at", "3650d"],
        5,
        "OPERATION_NOT_RETAINED",
    )
    .await;
    assert!(
        error["hint"]
            .as_str()
            .unwrap()
            .contains("operation log starts at"),
        "{error}"
    );
    daemon.stop().await;
}
