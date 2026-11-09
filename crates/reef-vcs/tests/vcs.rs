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

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::Duration;

use assert_cmd::Command;
use assert_cmd::assert::OutputAssertExt as _;
use reef_api::v1::{self, CreateWorkspaceRequest, RemoteScope, create_workspace_request};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_testing::synth::Preset;
use serde_json::Value;

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
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
        "AI_AGENT",
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "REEF_MODEL",
        "REEF_TASK",
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
/// in-process daemon needs, and checks its output against `reef schema`
/// (R2.24).
async fn run(mut command: Command, args: &[&str]) -> Output {
    command.args(args);
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    common::schema::check(args, &output);
    output
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
    // The index (R2.23): the fixture attaches watches inside the create.
    assert_eq!(ws_status["source"], "index", "{ws_status}");
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

    // `--follow` ends when nobody reads any more. The binary is quoted: a
    // target dir under a Reef workspace has a space in its path.
    let head = head_op(&daemon, &dir).await;
    ok(&daemon, &dir, &["describe", "-m", "piped"]).await;
    let script = format!(
        "'{}' --no-autostart op watch --after {head} --follow -o json | head -1",
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
        "set -o pipefail; '{}' --no-autostart presence watch --follow -o json | head -1",
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

/// `reef history -o text` says who wrote each change: its agents, or its
/// author's email when no agent wrote it, as for a commit imported from git
/// (#115).
#[tokio::test(flavor = "multi_thread")]
async fn history_text_names_authors() {
    let daemon = TestDaemon::start_with(|config| {
        no_automatic_snapshots(config);
    })
    .await;
    daemon
        .handle
        .state()
        .history
        .set_throttle(Duration::ZERO, Duration::ZERO);
    let plain = daemon.tmp.path().join("plain-git");
    std::fs::create_dir_all(&plain).unwrap();
    let plain = plain.canonicalize().unwrap();
    let first = reef_testing::git::init_repo(&plain, &[("README", "plain git\n")]);
    // A commit nobody wrote through Reef, which registering imports, as the
    // token benchmark's fixture lands its history.
    reef_testing::git::commit(
        &plain,
        "refs/heads/main",
        Some(&first),
        &[("README", "plain git\n"), ("landed.txt", "landed\n")],
    );
    ok(&daemon, &plain, &["repo", "register", "-o", "json"]).await;

    let created = ok(&daemon, &plain, &["ws", "create", "-o", "json"]).await;
    let ws = PathBuf::from(created["path"].as_str().unwrap());
    std::fs::write(ws.join("agent.txt"), "agent\n").unwrap();
    ok(&daemon, &ws, &["--agent", "bot-1", "status", "-o", "json"]).await;

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let landed = loop {
        let landed = ok(&daemon, &plain, &["history", "landed.txt", "-o", "json"]).await;
        let agent = ok(&daemon, &plain, &["history", "agent.txt", "-o", "json"]).await;
        let current = |found: &Value| {
            found["changes"]
                .as_array()
                .is_some_and(|c| !c.is_empty() && c.iter().all(|c| c["touches_state"] == "current"))
        };
        // And bot-1's snapshot in the activity: once failed on ubuntu with
        // the change current but only the owner listed (2026-11-07).
        let by_bot = agent["changes"][0]["activity"]
            .as_array()
            .is_some_and(|a| a.iter().any(|a| a["agent"] == "bot-1"));
        if current(&landed) && current(&agent) && by_bot {
            break landed;
        }
        if std::time::Instant::now() >= deadline {
            let ops = stdout(&run_in(&daemon, &plain, &["op", "log", "-o", "json"]).await);
            panic!("{landed}\n{agent}\noperations: {ops}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    let change = &landed["changes"][0];
    assert_eq!(change["author_email"], "reef-test@example.com", "{landed}");
    assert!(
        change["activity"]
            .as_array()
            .unwrap()
            .iter()
            .all(|a| a["agent"].is_null()),
        "{landed}"
    );

    let text = stdout(&run_in(&daemon, &plain, &["history", "landed.txt", "-o", "text"]).await);
    assert!(
        text.lines()
            .any(|line| line.contains(" <reef-test@example.com> ")),
        "{text}"
    );
    assert!(!text.contains("  -  "), "{text}");
    let text = stdout(&run_in(&daemon, &plain, &["history", "agent.txt", "-o", "text"]).await);
    let line = text.lines().next().unwrap();
    assert!(line.contains(" bot-1"), "{text}");
    // Agents only, never the author; dropping an agent-less row's `-` beside
    // them is the unit tests' (this change's rows all name an agent).
    assert!(
        !line.contains('<') && !line.contains("-,") && !line.contains(", -"),
        "{text}"
    );
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

// #126 test 6: brief and text `status` and `diff` read at a named operation
// name it; reads at head print what they did.
#[tokio::test(flavor = "multi_thread")]
async fn brief_names_the_operation_read() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    write(&ws, "one.txt", "1\n");
    ok(&daemon, &dir, &["ws", "snapshot"]).await;
    let x = head_op(&daemon, &dir).await;
    write(&ws, "two.txt", "2\n");
    ok(&daemon, &dir, &["ws", "snapshot"]).await;
    assert_ne!(head_op(&daemon, &dir).await, x);

    // Workspace scope, from a prefix: the full id, first.
    let output = run_in(&daemon, &dir, &["status", "--at-op", &x[..12]]).await;
    let status = json(&output);
    assert_eq!(status["at_operation"], x.as_str());
    assert!(
        stdout(&output).starts_with(&format!("{{\"at_operation\":\"{x}\",")),
        "{}",
        stdout(&output)
    );
    assert_eq!(status["workspace_status"]["source"], "operation");
    assert!(stderr(&output).is_empty(), "{}", stderr(&output));

    // Repository scope.
    let status = ok(
        &daemon,
        &repo.root,
        &["status", "--repo", &repo.id, "--at-op", &x],
    )
    .await;
    assert_eq!(status["at_operation"], x.as_str());
    assert!(status.get("workspace_status").is_none(), "{status}");

    // `--at` names it too, and still notes it on stderr.
    let output = run_in(&daemon, &dir, &["status", "--at", "0s"]).await;
    assert_eq!(
        json(&output)["at_operation"],
        head_op(&daemon, &dir).await.as_str()
    );
    assert!(
        stderr(&output).starts_with("reef: at operation "),
        "{}",
        stderr(&output)
    );

    // At head, byte for byte what it was, less the key.
    let h = head_op(&daemon, &dir).await;
    let head = stdout(&run_in(&daemon, &repo.root, &["status", "--repo", &repo.id]).await);
    let named = stdout(
        &run_in(
            &daemon,
            &repo.root,
            &["status", "--repo", &repo.id, "--at-op", &h],
        )
        .await,
    );
    assert!(!head.contains("\"at_operation\":"), "{head}");
    assert_eq!(
        named.replacen(&format!("\"at_operation\":\"{h}\","), "", 1),
        head
    );

    // Diff: lossless at a named operation; no key at head.
    let brief = ok(&daemon, &dir, &["diff", "--at-op", &x]).await;
    assert_eq!(brief["at_operation"], x.as_str());
    let full = ok(&daemon, &dir, &["-o", "json", "diff", "--at-op", &x]).await;
    assert_brief_of(&brief, &full, "diff");
    let head = stdout(&run_in(&daemon, &dir, &["diff"]).await);
    assert!(!head.contains("\"at_operation\":"), "{head}");

    // `ws list` is unchanged: an array, with no key.
    let listed = stdout(&run_in(&daemon, &dir, &["ws", "list", "--at-op", &x]).await);
    assert!(listed.starts_with('['), "{listed}");
    assert!(!listed.contains("\"at_operation\":"), "{listed}");

    // Text names it in a first line; at head it does not.
    let text = stdout(&run_in(&daemon, &dir, &["-o", "text", "status", "--at-op", &x]).await);
    assert!(
        text.starts_with(&format!("At operation {} (", &x[..12])),
        "{text}"
    );
    let text = stdout(&run_in(&daemon, &dir, &["-o", "text", "diff", "--at-op", &x]).await);
    assert!(
        text.starts_with(&format!("At operation {}\ndiff --git ", &x[..12])),
        "{text}"
    );
    for args in [&["-o", "text", "status"][..], &["-o", "text", "diff"]] {
        let text = stdout(&run_in(&daemon, &dir, args).await);
        assert!(!text.contains("At operation"), "{text}");
    }
    daemon.stop().await;
}

/// A `BROWSER` that records its arguments, one per line, in `log`.
fn browser_recorder(dir: &Path, log: &Path) -> PathBuf {
    let script = dir.join("browser");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nfor arg in \"$@\"; do echo \"$arg\" >> '{}'; done\n",
            log.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    script
}

// R2.18 18: `reef ui` opens nothing unless asked, never puts the token on a
// command line, and reports a disabled or placeholder web UI.
#[tokio::test(flavor = "multi_thread")]
async fn ui_command() {
    let daemon = TestDaemon::start().await;
    let dir = daemon.state_dir().to_owned();
    let tmp = tempfile::tempdir().unwrap();
    let log = tmp.path().join("opened");
    let browser = browser_recorder(tmp.path(), &log);

    // Piped: nothing opened and no credential, but a launch file the
    // caller can hand to the user.
    let output = run_in(&daemon, &dir, &["ui", "-o", "json"]).await;
    assert!(
        !stdout(&output).contains("#session="),
        "{}",
        stdout(&output)
    );
    let value = json(&output);
    let url = value["url"].as_str().unwrap().to_owned();
    assert!(url.starts_with("http://127.0.0.1:"), "{value}");
    let launch_path = Path::new(value["launch_path"].as_str().unwrap());
    assert!(
        launch_path.starts_with(daemon.state_dir().join("ui")),
        "{value}"
    );
    assert!(launch_path.is_file(), "{value}");
    assert_eq!(value["opened"], false, "{value}");
    assert!(value.get("session_url").is_none(), "{value}");
    // The same small object in every JSON mode.
    let brief = json(&run_in(&daemon, &dir, &["ui"]).await);
    assert_eq!(
        brief.as_object().unwrap().keys().collect::<Vec<_>>(),
        value.as_object().unwrap().keys().collect::<Vec<_>>()
    );

    // `--print`: the session URL.
    let value = ok(&daemon, &dir, &["ui", "--print"]).await;
    let session_url = value["session_url"].as_str().unwrap();
    assert!(
        session_url.starts_with(&format!("{url}#session=")),
        "{value}"
    );
    assert_eq!(value["opened"], false, "{value}");
    assert!(value["launch_path"].is_null(), "no launch file: {value}");

    // `--open`: the launch file's path, and no URL, on the opener's
    // command line.
    let mut command = reef(&daemon, &dir);
    command.env("BROWSER", &browser);
    let value = json(&run(command, &["ui", "--open"]).await);
    assert_eq!(value["opened"], true, "{value}");
    let launch_path = value["launch_path"].as_str().unwrap();
    assert!(
        Path::new(launch_path).starts_with(daemon.state_dir().join("ui")),
        "{value}"
    );
    assert!(value.get("session_url").is_none(), "{value}");
    let args = std::fs::read_to_string(&log).unwrap();
    assert_eq!(args, format!("{launch_path}\n"));
    assert!(!args.contains("http"), "{args}");
    daemon.stop().await;

    // `[ui] enabled = false`.
    let daemon = TestDaemon::start_with(|config| config.file.ui.enabled = false).await;
    let dir = daemon.state_dir().to_owned();
    let error = err(&daemon, &dir, &["ui"], 5, "WEB_UI_DISABLED").await;
    assert!(
        error["hint"].as_str().unwrap().contains("[ui] enabled"),
        "{error}"
    );
    daemon.stop().await;

    // A daemon that embeds only the placeholder: refused, and nothing is
    // opened even with `--open`.
    std::fs::remove_file(&log).unwrap();
    let daemon = TestDaemon::start_with(|config| config.web.assets_built = false).await;
    let dir = daemon.state_dir().to_owned();
    let mut command = reef(&daemon, &dir);
    command.env("BROWSER", &browser);
    let error = failed(
        &run(command, &["ui", "--open"]).await,
        5,
        "WEB_UI_NOT_BUILT",
    );
    let hint = error["hint"].as_str().unwrap();
    assert!(hint.contains("just web-build"), "{hint}");
    assert!(hint.contains("releases"), "{hint}");
    assert!(!log.exists(), "the browser was run");
    daemon.stop().await;
}

/// Stdout's JSON error object, after asserting the exit code.
fn refused(output: &Output, code: i32) -> Value {
    output.clone().assert().code(code);
    let value: Value = serde_json::from_str(&stdout(output))
        .unwrap_or_else(|e| panic!("{e}: {:?} {}", stdout(output), stderr(output)));
    value["error"].clone()
}

// R3.01 test 16: keys, adds signed here and elsewhere, listing, showing,
// the envelope's bytes, and a key file others can read.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one key through every command")]
async fn evidence_cli() {
    use reef_daemon::client::evidence::{self as format, PublicKey, SigningKey};

    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    let keys = daemon.tmp.path().join("keys");
    std::fs::create_dir(&keys).unwrap();
    let key = keys.join("k.pem");
    let key_text = key.to_str().unwrap();
    let public = keys.join("k.pem.pub");

    // A key pair: the private key 0600, the public key beside it.
    let generated = ok(&daemon, &dir, &["evidence", "key", "generate", key_text]).await;
    assert_eq!(
        std::fs::metadata(&key).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let public_pem = std::fs::read_to_string(&public).unwrap();
    let key_id = PublicKey::from_pem(&public_pem).unwrap().id().to_string();
    assert_eq!(generated["key_id"], key_id.as_str());
    assert_eq!(generated["public_key_path"], public.to_str().unwrap());
    // Never overwritten.
    let before = (std::fs::read(&key).unwrap(), public_pem.clone());
    let again = run_in(&daemon, &dir, &["evidence", "key", "generate", key_text]).await;
    let error = refused(&again, 4);
    assert!(
        error["message"].as_str().unwrap().contains("exists"),
        "{error}"
    );
    assert_eq!(
        (
            std::fs::read(&key).unwrap(),
            std::fs::read_to_string(&public).unwrap()
        ),
        before
    );

    // Add on `@`'s tree, with an artifact recorded by SHA-256.
    let log = keys.join("test.log");
    std::fs::write(&log, "ok 1 - parses\n").unwrap();
    let added = ok(
        &daemon,
        &dir,
        &[
            "-o",
            "json",
            "evidence",
            "add",
            "--kind",
            "test",
            "--result",
            "pass",
            "--key",
            key_text,
            "--artifact",
            log.to_str().unwrap(),
            "--annotation",
            "runner=local",
        ],
    )
    .await;
    assert_eq!(added["created"], true);
    let evidence = &added["evidence"];
    let id = evidence["id"].as_str().unwrap().to_owned();
    assert_eq!(evidence["key_id"], key_id.as_str());
    assert_eq!(evidence["signature"], "valid");
    assert_eq!(evidence["artifacts"][0]["name"], "test.log");
    assert_eq!(
        evidence["artifacts"][0]["sha256"],
        format::sha256_hex(b"ok 1 - parses\n").as_str()
    );
    let shown = ok(&daemon, &dir, &["-o", "json", "show"]).await;
    let tree = shown["tree_id"].as_str().unwrap().to_owned();
    assert_eq!(evidence["tree_id"], tree.as_str());
    assert_eq!(evidence["change_ids"][0], shown["change_id"]);
    // The piped default.
    let brief_add = ok(
        &daemon,
        &dir,
        &[
            "evidence", "add", "--kind", "lint", "--result", "fail", "--key", key_text,
        ],
    )
    .await;
    assert_eq!(brief_add["created"], true);

    // List the change's tree, in each form.
    let listed = ok(
        &daemon,
        &dir,
        &["-o", "json", "evidence", "list", "-r", "@"],
    )
    .await;
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&id.as_str()), "{listed}");
    let brief = ok(&daemon, &dir, &["evidence", "list", "-r", "@"]).await;
    assert_eq!(brief.as_array().unwrap().len(), 2, "{brief}");
    assert!(brief[0].get("tree_id").is_none(), "{brief}");
    run_in(
        &daemon,
        &dir,
        &["-o", "brief", "evidence", "list", "-r", "@"],
    )
    .await;
    let text = run_in(&daemon, &dir, &["-o", "text", "evidence", "list"]).await;
    let text = stdout(&text);
    assert!(
        text.lines()
            .any(|line| line.starts_with(&id[..12])
                && line.contains(&format!("key {}", &key_id[..12]))),
        "{text}"
    );

    // Show by prefix: text says the signature verifies, exit 0.
    let text = run_in(&daemon, &dir, &["-o", "text", "evidence", "show", &id[..8]]).await;
    assert!(text.status.success());
    assert!(
        stdout(&text).contains("signature: valid"),
        "{}",
        stdout(&text)
    );
    // Review F11: the statement as signed, keys in the payload's order.
    assert!(
        stdout(&text)
            .contains(r#"{"_type":"https://in-toto.io/Statement/v1","subject":[{"name":"tree""#),
        "{}",
        stdout(&text)
    );
    let full = ok(&daemon, &dir, &["-o", "json", "evidence", "show", &id[..8]]).await;
    assert_eq!(full["statement"]["predicate"]["kind"], "test");
    assert_eq!(
        full["statement"]["predicate"]["annotations"]["runner"],
        "local"
    );
    assert_eq!(full["envelope"]["payloadType"], format::PAYLOAD_TYPE);
    // `--envelope` is the bytes whose SHA-256 is the id.
    let raw = run_in(&daemon, &dir, &["evidence", "show", &id, "--envelope"]).await;
    assert!(raw.status.success());
    assert_eq!(format::sha256_hex(&raw.stdout), id);

    // An envelope signed elsewhere.
    let signer = SigningKey::from_seed(&[9; 32]).unwrap();
    let claim = format::Claim {
        kind: "build".into(),
        result: format::Outcome::Error,
        target: Some("//app:bin".into()),
        started_on: std::time::SystemTime::now(),
        finished_on: std::time::SystemTime::now(),
        artifacts: Vec::new(),
        annotations: serde_json::Map::new(),
    };
    let envelope = claim
        .sign(&format::TreeDigest::git(&tree).unwrap(), &signer)
        .unwrap();
    let envelope_file = keys.join("e.json");
    std::fs::write(&envelope_file, envelope.to_bytes()).unwrap();
    let signer_pem = keys.join("signer.pub.pem");
    std::fs::write(&signer_pem, signer.public_key().to_pem()).unwrap();
    let elsewhere = ok(
        &daemon,
        &dir,
        &[
            "-o",
            "json",
            "evidence",
            "add",
            "--envelope",
            envelope_file.to_str().unwrap(),
            "--public-key",
            signer_pem.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(elsewhere["created"], true);
    assert_eq!(
        elsewhere["evidence"]["key_id"],
        signer.public_key().id().as_str()
    );
    assert_eq!(elsewhere["evidence"]["result"], "error");
    assert_eq!(
        elsewhere["evidence"]["change_ids"],
        serde_json::json!([]),
        "no -r: added by tree alone"
    );

    // Review F2: a time before 1970 is refused with a hint, not a panic.
    let early = run_in(
        &daemon,
        &dir,
        &[
            "evidence",
            "add",
            "--kind",
            "test",
            "--result",
            "pass",
            "--key",
            key_text,
            "--started",
            "100y",
        ],
    )
    .await;
    let error = refused(&early, 2);
    assert!(error["hint"].as_str().unwrap().contains("1970"), "{error}");
    // Review F7: a key id as the text prints it (12 digits) is refused.
    let short = run_in(&daemon, &dir, &["evidence", "list", "--key", &key_id[..12]]).await;
    let error = refused(&short, 2);
    assert!(
        error["hint"].as_str().unwrap().contains("full key id"),
        "{error}"
    );
    let full = ok(
        &daemon,
        &dir,
        &["-o", "json", "evidence", "list", "--key", &key_id],
    )
    .await;
    assert_eq!(full.as_array().unwrap().len(), 2, "{full}");

    // A key others can read is refused before anything is sent.
    let counts = daemon.handle.state().db.evidence_counts().unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o644)).unwrap();
    let open = run_in(
        &daemon,
        &dir,
        &[
            "evidence", "add", "--kind", "test", "--result", "pass", "--key", key_text,
        ],
    )
    .await;
    let error = refused(&open, 7);
    assert_eq!(
        error["hint"].as_str().unwrap(),
        format!("chmod 600 {key_text}"),
        "{error}"
    );
    assert_eq!(daemon.handle.state().db.evidence_counts().unwrap(), counts);
    // Review F12: the key is checked before connecting, so with no daemon
    // the refusal is the key's, not "daemon is not running".
    let nowhere = tempfile::tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    command
        .env_remove("REEF_SOCKET")
        .env("REEF_STATE_DIR", nowhere.path().join("state"))
        .current_dir(&dir)
        .args([
            "--no-autostart",
            "evidence",
            "add",
            "--kind",
            "test",
            "--result",
            "pass",
            "--key",
            key_text,
        ]);
    let offline = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    let error = refused(&offline, 7);
    assert_eq!(error["hint"], format!("chmod 600 {key_text}"), "{error}");
    daemon.stop().await;
}

// R3.02 test 17: `add --artifact` uploads before it adds; `--no-upload`
// records the digest only; an envelope's artifacts are checked and
// uploaded; `artifact upload`, `show` and `read`; and `ws gc --dry-run`'s
// counts. Every run's output is checked against `reef schema`.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one artifact through every command")]
async fn evidence_cli_artifacts() {
    use reef_daemon::client::evidence::{self as format, SigningKey};

    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    let files = daemon.tmp.path().join("files");
    std::fs::create_dir(&files).unwrap();
    let key = files.join("k.pem");
    let key_text = key.to_str().unwrap();
    ok(&daemon, &dir, &["evidence", "key", "generate", key_text]).await;
    let log = files.join("log.txt");
    let log_bytes = b"ok 1 - uploads\n".repeat(1000);
    std::fs::write(&log, &log_bytes).unwrap();
    let log_sha = format::sha256_hex(&log_bytes);
    let add = |extra: &'static [&'static str]| {
        let mut args = vec![
            "-o", "json", "evidence", "add", "--kind", "test", "--result", "pass", "--key",
        ];
        args.push(key_text);
        args.extend_from_slice(extra);
        args
    };

    // Uploaded, then added: `show` prints it stored.
    let mut args = add(&["--target", "uploaded"]);
    args.extend(["--artifact", log.to_str().unwrap()]);
    let added = ok(&daemon, &dir, &args).await;
    let artifact = &added["evidence"]["artifacts"][0];
    assert_eq!(artifact["state"], "stored", "{added}");
    assert_eq!(artifact["size"], log_bytes.len());
    let id = added["evidence"]["id"].as_str().unwrap().to_owned();
    let text = run_in(&daemon, &dir, &["-o", "text", "evidence", "show", &id]).await;
    let line = format!("log.txt sha256:{} stored 15 KiB", &log_sha[..12]);
    assert!(stdout(&text).contains(&line), "{}", stdout(&text));

    // `--no-upload`: the digest only.
    let other = files.join("other.txt");
    std::fs::write(&other, "not uploaded\n").unwrap();
    let mut args = add(&["--target", "digest-only", "--no-upload"]);
    args.extend(["--artifact", other.to_str().unwrap()]);
    let added = ok(&daemon, &dir, &args).await;
    assert_eq!(added["evidence"]["artifacts"][0]["state"], "absent");
    let text = run_in(
        &daemon,
        &dir,
        &[
            "-o",
            "text",
            "evidence",
            "show",
            added["evidence"]["id"].as_str().unwrap(),
        ],
    )
    .await;
    assert!(
        stdout(&text).contains("other.txt sha256:"),
        "{}",
        stdout(&text)
    );
    assert!(stdout(&text).contains(" absent"), "{}", stdout(&text));

    // Bytes that are not what the claim's digest was hashed from, as a file
    // changed during its upload leaves (here a partial upload of other bytes
    // under its digest, which the upload resumes): ARTIFACT_DIGEST_MISMATCH,
    // and no evidence is added.
    let changed = files.join("changed.txt");
    let changed_bytes = b"the bytes the claim names\n".repeat(10);
    std::fs::write(&changed, &changed_bytes).unwrap();
    let incoming = daemon
        .handle
        .state()
        .artifacts
        .dir(repo.id.parse().unwrap())
        .join("incoming");
    std::fs::create_dir_all(&incoming).unwrap();
    let partial = incoming.join(format!(
        "{}.{}",
        format::sha256_hex(&changed_bytes),
        changed_bytes.len()
    ));
    std::fs::write(&partial, b"other bytes").unwrap();
    let counts = daemon.handle.state().db.evidence_counts().unwrap();
    let mut args = add(&["--target", "changed"]);
    args.extend(["--artifact", changed.to_str().unwrap()]);
    let output = run_in(&daemon, &dir, &args).await;
    failed(&output, 2, "ARTIFACT_DIGEST_MISMATCH");
    assert_eq!(
        daemon.handle.state().db.evidence_counts().unwrap(),
        counts,
        "nothing was added"
    );
    assert!(!partial.exists(), "the partial upload was deleted");

    // An envelope signed elsewhere, naming one file: that file uploads, and
    // a file it does not name is refused with nothing sent.
    let shown = ok(&daemon, &dir, &["-o", "json", "show"]).await;
    let tree = shown["tree_id"].as_str().unwrap().to_owned();
    let report = files.join("report.xml");
    std::fs::write(&report, "<report/>\n").unwrap();
    let signer = SigningKey::from_seed(&[11; 32]).unwrap();
    let claim = format::Claim {
        kind: "build".into(),
        result: format::Outcome::Pass,
        target: None,
        started_on: std::time::SystemTime::now(),
        finished_on: std::time::SystemTime::now(),
        artifacts: vec![format::Artifact {
            name: "report.xml".into(),
            sha256: format::sha256_hex(b"<report/>\n"),
            uri: None,
            media_type: None,
        }],
        annotations: serde_json::Map::new(),
    };
    let envelope = claim
        .sign(&format::TreeDigest::git(&tree).unwrap(), &signer)
        .unwrap();
    let envelope_file = files.join("e.json");
    std::fs::write(&envelope_file, envelope.to_bytes()).unwrap();
    let signer_pem = files.join("signer.pub.pem");
    std::fs::write(&signer_pem, signer.public_key().to_pem()).unwrap();
    let counts = daemon.handle.state().db.evidence_counts().unwrap();
    let artifacts_before = daemon.handle.state().db.artifact_counts().unwrap();
    let refused_add = run_in(
        &daemon,
        &dir,
        &[
            "evidence",
            "add",
            "--envelope",
            envelope_file.to_str().unwrap(),
            "--public-key",
            signer_pem.to_str().unwrap(),
            "--artifact",
            other.to_str().unwrap(),
        ],
    )
    .await;
    let error = refused(&refused_add, 2);
    assert!(
        error["message"].as_str().unwrap().contains("other.txt"),
        "{error}"
    );
    assert_eq!(daemon.handle.state().db.evidence_counts().unwrap(), counts);
    assert_eq!(
        daemon.handle.state().db.artifact_counts().unwrap(),
        artifacts_before,
        "nothing was sent"
    );
    let elsewhere = ok(
        &daemon,
        &dir,
        &[
            "-o",
            "json",
            "evidence",
            "add",
            "--envelope",
            envelope_file.to_str().unwrap(),
            "--public-key",
            signer_pem.to_str().unwrap(),
            "--artifact",
            report.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(elsewhere["evidence"]["artifacts"][0]["state"], "stored");

    // `artifact upload`: a line each, then `already stored`.
    let second = files.join("second.bin");
    std::fs::write(&second, vec![7u8; 3 << 20]).unwrap();
    let paths = [log.to_str().unwrap(), second.to_str().unwrap()];
    let first = run_in(
        &daemon,
        &dir,
        &[
            "-o", "text", "evidence", "artifact", "upload", paths[0], paths[1],
        ],
    )
    .await;
    let lines: Vec<String> = stdout(&first).lines().map(str::to_owned).collect();
    assert_eq!(lines.len(), 2, "{lines:?}");
    assert!(lines[0].ends_with("already stored"), "{lines:?}");
    assert!(lines[1].starts_with(&format::sha256_hex(&vec![7u8; 3 << 20])[..12]));
    assert!(lines[1].ends_with(" 3.0 MiB stored"), "{lines:?}");
    let again = run_in(
        &daemon,
        &dir,
        &["-o", "text", "evidence", "artifact", "upload", paths[1]],
    )
    .await;
    assert!(stdout(&again).trim_end().ends_with("already stored"));
    let listed = ok(
        &daemon,
        &dir,
        &["-o", "json", "evidence", "artifact", "upload", paths[1]],
    )
    .await;
    assert_eq!(listed[0]["created"], false);
    assert_eq!(listed[0]["path"], paths[1]);
    run_in(&daemon, &dir, &["evidence", "artifact", "upload", paths[1]]).await;

    // `artifact read <prefix>` is the bytes; with -o json, the record.
    let read = run_in(
        &daemon,
        &dir,
        &["evidence", "artifact", "read", &log_sha[..8]],
    )
    .await;
    assert!(read.status.success(), "{}", stderr(&read));
    assert_eq!(format::sha256_hex(&read.stdout), log_sha);
    let ranged = run_in(
        &daemon,
        &dir,
        &[
            "evidence", "artifact", "read", &log_sha, "--offset", "15", "--length", "15",
        ],
    )
    .await;
    assert_eq!(ranged.stdout, &log_bytes[15..30]);
    let record = ok(
        &daemon,
        &dir,
        &["-o", "json", "evidence", "artifact", "read", &log_sha],
    )
    .await;
    assert_eq!(record["sha256"], log_sha.as_str());
    assert_eq!(record["state"], "stored");
    run_in(
        &daemon,
        &dir,
        &["-o", "brief", "evidence", "artifact", "read", &log_sha],
    )
    .await;

    // `artifact show` lists the evidence naming it.
    let shown = ok(
        &daemon,
        &dir,
        &["-o", "json", "evidence", "artifact", "show", &log_sha[..8]],
    )
    .await;
    assert_eq!(shown["evidence_count"], 1);
    assert_eq!(shown["evidence_ids"][0], id.as_str());
    let brief = ok(&daemon, &dir, &["evidence", "artifact", "show", &log_sha]).await;
    assert!(brief.get("evidence_ids").is_none(), "{brief}");
    let text = run_in(
        &daemon,
        &dir,
        &["-o", "text", "evidence", "artifact", "show", &log_sha],
    )
    .await;
    assert!(stdout(&text).contains(&id[..12]), "{}", stdout(&text));
    // `read -o json` prints the record as `show` reads it, absent too;
    // reading the bytes of one never uploaded is ARTIFACT_NOT_FOUND.
    let absent_sha = format::sha256_hex(b"not uploaded\n");
    let record = ok(
        &daemon,
        &dir,
        &["-o", "json", "evidence", "artifact", "read", &absent_sha],
    )
    .await;
    assert_eq!(record["state"], "absent", "{record}");
    let absent = run_in(
        &daemon,
        &dir,
        &["-o", "text", "evidence", "artifact", "read", &absent_sha],
    )
    .await;
    absent.clone().assert().code(3);
    assert!(
        stderr(&absent).contains("never stored"),
        "{}",
        stderr(&absent)
    );

    // A damaged read: the bytes, then the error, and stderr says so.
    let stored = daemon
        .handle
        .state()
        .artifacts
        .dir(repo.id.parse().unwrap())
        .join("sha256")
        .join(&log_sha[..2])
        .join(&log_sha[2..]);
    std::fs::set_permissions(&stored, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut tampered = log_bytes.clone();
    tampered[100] ^= 1;
    std::fs::write(&stored, &tampered).unwrap();
    let damaged = run_in(
        &daemon,
        &dir,
        &["-o", "text", "evidence", "artifact", "read", &log_sha],
    )
    .await;
    assert!(!damaged.status.success());
    assert_eq!(damaged.stdout, tampered);
    assert!(
        stderr(&damaged).contains("not the artifact"),
        "{}",
        stderr(&damaged)
    );
    // Dropped as damaged: it now reads as collected.
    let record = ok(
        &daemon,
        &dir,
        &["-o", "json", "evidence", "artifact", "show", &log_sha],
    )
    .await;
    assert_eq!(record["state"], "collected", "{record}");

    // `ws gc --dry-run` prints the new counts.
    let gc = run_in(&daemon, &dir, &["-o", "text", "ws", "gc", "--dry-run"]).await;
    assert!(gc.status.success(), "{}", stderr(&gc));
    assert!(
        stdout(&gc).contains("evidence 0 removed, artifacts 0 collected (0 B)"),
        "{}",
        stdout(&gc)
    );
    let reports = ok(&daemon, &dir, &["-o", "json", "ws", "gc", "--dry-run"]).await;
    assert!(
        reports[0]["artifact_bytes"].as_u64().unwrap() > 3 << 20,
        "{reports}"
    );
    daemon.stop().await;
}

/// Every file under `dir`, recursively, for a scan before and after.
fn files_under(dir: &Path) -> std::collections::BTreeSet<PathBuf> {
    let mut files = std::collections::BTreeSet::new();
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => dirs.push(entry.path()),
                Ok(kind) if kind.is_file() => {
                    files.insert(entry.path());
                }
                _ => {}
            }
        }
    }
    files
}

// R3.03 test 14: every `reef identity` command, its JSON, brief and piped
// output checked against `reef schema` by `run`; a private key that signs
// and a certificate that checks; no private key in the text form.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one identity through every command")]
async fn identity_cli() {
    use reef_daemon::client::evidence::session::{Authority, IssuerKind, check_certificate};
    use reef_daemon::client::evidence::{PAYLOAD_TYPE, PublicKey, SigningKey, Verified};

    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    let login = reef_daemon::identity::default_name(&reef_daemon::client::os_username());

    let created = ok(
        &daemon,
        &dir,
        &[
            "-o", "json", "identity", "create", "ci-main", "--kind", "ci",
        ],
    )
    .await;
    assert_eq!(created["kind"], "ci");
    ok(
        &daemon,
        &dir,
        &["identity", "create", "dana", "--kind", "person"],
    )
    .await;
    let listed = ok(&daemon, &dir, &["-o", "json", "identity", "list"]).await;
    assert_eq!(listed.as_array().unwrap().len(), 3);
    let brief = run_in(&daemon, &dir, &["identity", "list"]).await;
    assert!(
        stdout(&brief).contains("\"name\":\"ci-main\""),
        "{}",
        stdout(&brief)
    );
    let shown = ok(&daemon, &dir, &["-o", "json", "identity", "show"]).await;
    assert_eq!(shown["name"], login.as_str());
    assert_eq!(shown["is_default"], true);
    run_in(&daemon, &dir, &["-o", "brief", "identity", "show", "dana"]).await;
    let text = run_in(&daemon, &dir, &["-o", "text", "identity", "show"]).await;
    assert!(stdout(&text).contains("default"), "{}", stdout(&text));

    // The authority: a PEM that parses.
    let authorities = ok(
        &daemon,
        &dir,
        &["-o", "json", "identity", "authority", "show"],
    )
    .await;
    let current = &authorities[0];
    assert_eq!(current["current"], true);
    let authority = PublicKey::from_pem(current["public_key"].as_str().unwrap()).unwrap();
    assert_eq!(authority.id().as_str(), current["key_id"]);
    let text = run_in(
        &daemon,
        &dir,
        &["-o", "text", "identity", "authority", "show"],
    )
    .await;
    assert!(stdout(&text).contains("-----BEGIN PUBLIC KEY-----"));

    // A key issued as JSON: its private key signs, its certificate checks.
    let issued = ok(
        &daemon,
        &dir,
        &[
            "-o", "json", "identity", "session", "issue", "--actor", "executor",
        ],
    )
    .await;
    assert_eq!(issued["key"]["identity"], login.as_str());
    assert_eq!(issued["key"]["actor"], "executor");
    let private = issued["private_key"].as_str().unwrap();
    assert!(private.starts_with("-----BEGIN PRIVATE KEY-----"));
    let key = SigningKey::from_pkcs8_pem(private).unwrap();
    assert_eq!(key.public_key().id().as_str(), issued["key"]["key_id"]);
    let signed = key.sign(PAYLOAD_TYPE, b"x").to_bytes();
    assert!(Verified::check(&signed, key.public_key().spki()).is_err());
    let certificate = serde_json::to_vec(&issued["certificate"]).unwrap();
    let known = |_: &reef_daemon::client::evidence::KeyId| {
        Some(Authority {
            key: authority.clone(),
            kind: IssuerKind::ReefDaemon,
            revocation: None,
        })
    };
    let certified = check_certificate(&certificate, &known).unwrap();
    assert_eq!(certified.binding.public_key, *key.public_key());
    let session = issued["key"]["session_id"].as_str().unwrap().to_owned();
    // Piped, brief: the private key too, for a script in another language.
    let piped = ok(&daemon, &dir, &["identity", "session", "issue"]).await;
    assert!(piped["private_key"].is_string(), "{piped}");
    // On a terminal, the text form: refused before a key is issued, since
    // its private key could not be printed (review finding 10).
    let text = run_in(
        &daemon,
        &dir,
        &["-o", "text", "identity", "session", "issue"],
    )
    .await;
    assert!(!text.status.success());
    assert!(!stdout(&text).contains("PRIVATE KEY"), "{}", stdout(&text));
    assert!(
        format!("{}{}", stdout(&text), stderr(&text)).contains("-o json"),
        "{} {}",
        stdout(&text),
        stderr(&text)
    );

    let keys = ok(
        &daemon,
        &dir,
        &["-o", "json", "identity", "session", "list"],
    )
    .await;
    assert_eq!(
        keys.as_array().unwrap().len(),
        2,
        "the text form issued none"
    );
    run_in(&daemon, &dir, &["identity", "session", "list", "--all"]).await;
    let key_id = issued["key"]["key_id"].as_str().unwrap();
    let one = ok(
        &daemon,
        &dir,
        &["-o", "json", "identity", "session", "show", &key_id[..8]],
    )
    .await;
    assert_eq!(one["certificate_valid"], true);
    run_in(&daemon, &dir, &["identity", "session", "show", key_id]).await;
    let revoked = ok(
        &daemon,
        &dir,
        &[
            "-o",
            "json",
            "identity",
            "session",
            "revoke",
            "--session",
            &session,
            "--since-issued",
        ],
    )
    .await;
    assert_eq!(revoked.as_array().unwrap().len(), 1);
    assert!(revoked[0]["revocation"].is_object());
    let live = ok(
        &daemon,
        &dir,
        &["-o", "json", "identity", "session", "list"],
    )
    .await;
    assert_eq!(live.as_array().unwrap().len(), 1);

    // Revoking an identity prints its linked tokens.
    daemon
        .client
        .create_remote_token_for("ci", RemoteScope::Write, None, "ci-main")
        .await
        .unwrap();
    let text = run_in(
        &daemon,
        &dir,
        &[
            "-o", "text", "identity", "revoke", "ci-main", "--reason", "rotated",
        ],
    )
    .await;
    text.clone().assert().success();
    assert!(
        stdout(&text).contains("reef remote token revoke ci"),
        "{}",
        stdout(&text)
    );
    let gone = ok(
        &daemon,
        &dir,
        &["-o", "json", "identity", "show", "ci-main"],
    )
    .await;
    assert_eq!(gone["revocation"]["reason"], "rotated");
    let error = err(
        &daemon,
        &dir,
        &["identity", "create", "ci-main", "--kind", "ci"],
        4,
        "IDENTITY_EXISTS",
    )
    .await;
    assert!(
        error["hint"].as_str().unwrap().contains("never reused"),
        "{error}"
    );

    // The authority, rotated and revoked.
    let rotated = ok(
        &daemon,
        &dir,
        &["-o", "json", "identity", "authority", "rotate", "--revoke"],
    )
    .await;
    assert_eq!(rotated["retired"]["key_id"], current["key_id"]);
    assert!(rotated["retired"]["revocation"].is_object());
    let second = ok(
        &daemon,
        &dir,
        &["-o", "json", "identity", "authority", "rotate"],
    )
    .await;
    let all = ok(
        &daemon,
        &dir,
        &["-o", "json", "identity", "authority", "show"],
    )
    .await;
    assert_eq!(all.as_array().unwrap().len(), 3);
    // A retired authority, revoked by key id prefix (review finding 4).
    let retired = second["retired"]["key_id"].as_str().unwrap();
    let revoked = ok(
        &daemon,
        &dir,
        &[
            "-o",
            "json",
            "identity",
            "authority",
            "revoke",
            &retired[..8],
            "--since-created",
            "--reason",
            "restored",
        ],
    )
    .await;
    assert_eq!(revoked["key_id"], retired);
    assert_eq!(revoked["revocation"]["reason"], "restored");
    let current = all[0]["key_id"].as_str().unwrap();
    err(
        &daemon,
        &dir,
        &["identity", "authority", "revoke", current],
        5,
        "AUTHORITY_IS_CURRENT",
    )
    .await;
    daemon.stop().await;
}

// R3.03 test 15: `evidence add` with no key signs with a session key and
// leaves no key file; `--actor human`; `--key` still works, unbound;
// `evidence list --identity`; and a remote token linked to a ci identity.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one claim through each signer")]
async fn evidence_cli_session() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    let login = reef_daemon::identity::default_name(&reef_daemon::client::os_username());
    let tmp_dir = std::env::temp_dir();
    let before = (files_under(&dir), files_under(&tmp_dir));

    let added = ok(
        &daemon,
        &dir,
        &[
            "-o", "json", "evidence", "add", "--kind", "test", "--result", "pass",
        ],
    )
    .await;
    let evidence = &added["evidence"];
    assert_eq!(evidence["identity"]["status"], "bound", "{evidence}");
    assert_eq!(evidence["identity"]["key"]["identity"], login.as_str());
    assert_eq!(evidence["identity"]["key"]["actor"], "agent");
    let window = |key: &Value| {
        let at = |field: &str| {
            humantime::parse_rfc3339(key[field].as_str().unwrap())
                .unwrap()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        };
        at("not_after") - at("not_before")
    };
    assert_eq!(window(&evidence["identity"]["key"]), 300, "five minutes");
    let id = evidence["id"].as_str().unwrap().to_owned();
    let text = run_in(&daemon, &dir, &["-o", "text", "evidence", "show", &id]).await;
    let shown = stdout(&text);
    assert!(
        shown.lines().any(|line| line.starts_with("identity:")
            && line.contains(&format!(" {login} (person) as agent"))),
        "{shown}"
    );
    assert!(shown.contains("policy decides whom to trust"), "{shown}");

    let human = ok(
        &daemon,
        &dir,
        &[
            "-o", "json", "evidence", "add", "--kind", "manual", "--result", "pass", "--actor",
            "human",
        ],
    )
    .await;
    let human_id = human["evidence"]["id"].as_str().unwrap().to_owned();
    let text = run_in(
        &daemon,
        &dir,
        &["-o", "text", "evidence", "show", &human_id],
    )
    .await;
    assert!(
        stdout(&text).contains("presence none (not verified)"),
        "{}",
        stdout(&text)
    );
    assert!(stdout(&text).contains("as human"), "{}", stdout(&text));

    // No session key file was left anywhere. A daemon's own authority key
    // (`<state>/identity/authority.pem`) is expected: this daemon's, or that of
    // another test's daemon running in parallel under the same temp dir.
    let after = (files_under(&dir), files_under(&tmp_dir));
    let is_authority = |path: &PathBuf| {
        path.file_name().is_some_and(|name| name == "authority.pem")
            && path
                .parent()
                .and_then(Path::file_name)
                .is_some_and(|name| name == "identity")
    };
    let new_files: Vec<&PathBuf> = after
        .0
        .difference(&before.0)
        .chain(after.1.difference(&before.1))
        .filter(|path| !is_authority(path))
        .filter(|path| std::fs::read_to_string(path).is_ok_and(|text| text.contains("PRIVATE KEY")))
        .collect();
    assert!(new_files.is_empty(), "{new_files:?}");

    // `--key` still works, and reads unbound; --identity conflicts with it.
    let keys = daemon.tmp.path().join("keys");
    std::fs::create_dir(&keys).unwrap();
    let key = keys.join("k.pem");
    let key_text = key.to_str().unwrap();
    ok(&daemon, &dir, &["evidence", "key", "generate", key_text]).await;
    let bare = ok(
        &daemon,
        &dir,
        &[
            "-o", "json", "evidence", "add", "--kind", "lint", "--result", "pass", "--key",
            key_text,
        ],
    )
    .await;
    assert_eq!(bare["evidence"]["identity"]["status"], "unbound");
    let bare_id = bare["evidence"]["id"].as_str().unwrap().to_owned();
    let text = run_in(&daemon, &dir, &["-o", "text", "evidence", "show", &bare_id]).await;
    assert!(stdout(&text).contains("unbound key"), "{}", stdout(&text));
    let conflict = run_in(
        &daemon,
        &dir,
        &[
            "evidence",
            "add",
            "--kind",
            "lint",
            "--result",
            "pass",
            "--key",
            key_text,
            "--identity",
            "x",
        ],
    )
    .await;
    conflict.clone().assert().code(2);

    // The list: `name:actor` for a bound key; the filter by identity.
    let text = run_in(&daemon, &dir, &["-o", "text", "evidence", "list"]).await;
    assert!(
        stdout(&text).contains(&format!("{login}:agent")),
        "{}",
        stdout(&text)
    );
    let brief = run_in(&daemon, &dir, &["evidence", "list"]).await;
    assert!(
        stdout(&brief).contains(&format!("\"identity\":\"{login}\"")),
        "{}",
        stdout(&brief)
    );
    let mine = ok(
        &daemon,
        &dir,
        &["-o", "json", "evidence", "list", "--identity", &login],
    )
    .await;
    assert_eq!(mine.as_array().unwrap().len(), 2);
    let nobody = ok(
        &daemon,
        &dir,
        &["-o", "json", "evidence", "list", "--identity", "nobody"],
    )
    .await;
    assert_eq!(nobody.as_array().unwrap().len(), 0);

    // A ci identity's key over the socket is refused, with the rule's
    // advice.
    ok(
        &daemon,
        &dir,
        &["identity", "create", "ci-main", "--kind", "ci"],
    )
    .await;
    let error = err(
        &daemon,
        &dir,
        &[
            "evidence",
            "add",
            "--kind",
            "test",
            "--result",
            "pass",
            "--identity",
            "ci-main",
        ],
        7,
        "SESSION_KEY_REFUSED",
    )
    .await;
    assert!(
        error["hint"].as_str().unwrap().contains("remote token"),
        "{error}"
    );
    // With artifacts to upload, a refusal is found before any upload, so
    // nothing is stored for evidence that is never added (review finding 9).
    let log = daemon.tmp.path().join("refused.log");
    std::fs::write(&log, "never uploaded\n").unwrap();
    let artifacts_before = daemon.handle.state().db.artifact_counts().unwrap();
    for (identity, code, reason) in [
        ("ci-main", 7, "SESSION_KEY_REFUSED"),
        ("nobody", 3, "IDENTITY_NOT_FOUND"),
    ] {
        err(
            &daemon,
            &dir,
            &[
                "evidence",
                "add",
                "--kind",
                "test",
                "--result",
                "pass",
                "--identity",
                identity,
                "--artifact",
                log.to_str().unwrap(),
            ],
            code,
            reason,
        )
        .await;
        assert_eq!(
            daemon.handle.state().db.artifact_counts().unwrap(),
            artifacts_before,
            "{identity}: nothing was uploaded"
        );
    }
    let linked = ok(
        &daemon,
        &dir,
        &[
            "-o",
            "json",
            "remote",
            "token",
            "create",
            "ci",
            "--scope",
            "write",
            "--identity",
            "ci-main",
        ],
    )
    .await;
    assert_eq!(linked["identity"], "ci-main");
    daemon.stop().await;
}

/// R3.04 test 16: `ws run` declares a model, a task and an instruction, its
/// command sees them and declares the next instruction, and `provenance
/// show`, `list` and `ws inspect` read them back. No instruction text
/// reaches the daemon.
#[tokio::test(flavor = "multi_thread")]
async fn provenance_cli() {
    use reef_daemon::client::evidence::sha256_hex;

    let (daemon, repo) = fixture().await;
    let files = daemon.tmp.path().join("prompts");
    std::fs::create_dir(&files).unwrap();
    let prompt = files.join("prompt.txt");
    let next = files.join("next.txt");
    std::fs::write(&prompt, "add a parser test\n").unwrap();
    std::fs::write(&next, "now make it pass\n").unwrap();
    let reef_bin = env!("CARGO_BIN_EXE_reef");
    let script = format!(
        "echo \"$REEF_MODEL $REEF_TASK\" > env.txt && {reef_bin} --no-autostart provenance declare -o json --instruction-file {} > /dev/null && echo y > g.txt",
        next.display()
    );
    let report = ok(
        &daemon,
        &repo.root,
        &[
            "-o",
            "json",
            "ws",
            "run",
            "--keep",
            "--model",
            "example/model-x",
            "--task",
            "T-1",
            "--instruction-file",
            prompt.to_str().unwrap(),
            "--",
            "sh",
            "-c",
            &script,
        ],
    )
    .await;
    assert_eq!(report["exit_code"], 0, "{report}");
    let ws_id = report["workspace_id"].as_str().unwrap().to_owned();
    let ws_dir = PathBuf::from(report["path"].as_str().unwrap());
    assert_eq!(
        std::fs::read_to_string(ws_dir.join("env.txt")).unwrap(),
        "example/model-x T-1\n"
    );

    // Two records at least: env.txt under the first instruction (the
    // declaration's snapshot), g.txt under the second (the final one).
    let listed = ok(
        &daemon,
        &ws_dir,
        &["-o", "json", "provenance", "list", "--mine", "--limit", "0"],
    )
    .await;
    let records = listed.as_array().unwrap();
    assert!(records.len() >= 2, "{listed}");
    let first = sha256_hex(b"add a parser test\n");
    let second = sha256_hex(b"now make it pass\n");
    let instructions: Vec<&str> = records
        .iter()
        .map(|record| record["context"]["instruction_sha256"].as_str().unwrap())
        .collect();
    assert_eq!(instructions.first(), Some(&second.as_str()), "{listed}");
    assert_eq!(instructions.last(), Some(&first.as_str()), "{listed}");
    for record in records {
        assert_eq!(record["context"]["model"], "example/model-x");
        assert_eq!(record["context"]["task"], "T-1");
        assert_eq!(record["signature"], "valid");
        assert_eq!(record["identity"]["status"], "bound");
        assert_eq!(record["identity"]["key"]["custody"], "daemon");
    }

    // `show` of `@`, in text and JSON; and of a describe-only commit, the
    // change's latest record.
    let shown = ok(&daemon, &ws_dir, &["-o", "json", "provenance", "show"]).await;
    assert_eq!(shown["context"]["instruction_sha256"], second.as_str());
    let text = stdout(&run_in(&daemon, &ws_dir, &["-o", "text", "provenance", "show"]).await);
    for line in [
        "signature: valid",
        "key held by the daemon",
        "declared: model example/model-x",
        "policy decides whom to trust",
    ] {
        assert!(text.contains(line), "{line}: {text}");
    }
    ok(
        &daemon,
        &ws_dir,
        &["-o", "json", "describe", "-m", "described"],
    )
    .await;
    let latest = ok(&daemon, &ws_dir, &["-o", "json", "provenance", "show"]).await;
    assert_eq!(latest["latest_of"], shown["change_id"], "{latest}");
    let brief = ok(&daemon, &ws_dir, &["provenance", "list", "--change", "@"]).await;
    assert!(brief[0]["context"]["model"].is_string(), "{brief}");

    // `ws inspect` names the session.
    let inspect = stdout(&run_in(&daemon, &ws_dir, &["-o", "text", "ws", "inspect", &ws_id]).await);
    assert!(inspect.contains("model example/model-x"), "{inspect}");

    // No instruction text in the daemon's database.
    let db = std::fs::read(daemon.state_dir().join("reef.sqlite3")).unwrap();
    for text in [&b"add a parser test"[..], &b"now make it pass"[..]] {
        assert!(!db.windows(text.len()).any(|w| w == text));
    }
}

/// An executor in this process for `job_cli`: claims one job (waiting),
/// reports progress, sends `result` for every pair, and completes `DONE`.
async fn serve_one(
    client: reef_daemon::client::Client,
    result: reef_daemon::client::evidence::Outcome,
) {
    use reef_daemon::client::evidence::{Claim, SigningKey, TreeDigest};

    let claimed = client
        .claim_job(v1::ClaimJobRequest {
            executor: Some(v1::ExecutorInfo {
                name: "cli-runner".into(),
                version: "1".into(),
                kinds: vec!["test".into()],
                labels: Vec::new(),
            }),
            protocol_version: 1,
            repos: Vec::new(),
            wait: Some(prost_types::Duration {
                seconds: 30,
                nanos: 0,
            }),
            lease: None,
        })
        .await
        .unwrap();
    let job = claimed.job.expect("a job to serve");
    let spec = job.spec.unwrap();
    let key = SigningKey::from_seed(&[5; 32]).unwrap();
    let started = std::time::SystemTime::UNIX_EPOCH
        + Duration::from_secs(
            std::time::SystemTime::now()
                .duration_since(std::time::SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs(),
        );
    client
        .update_job(v1::UpdateJobRequest {
            job_id: job.job_id.clone(),
            claim_token: claimed.claim_token.clone(),
            progress: Some(v1::JobProgress {
                phase: "test".into(),
                done: 0,
                total: 1,
                unit: "targets".into(),
            }),
            evidence: Vec::new(),
            release: false,
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    let evidence = spec
        .targets
        .iter()
        .map(|target| {
            let claim = Claim {
                kind: "test".into(),
                result,
                target: Some(target.clone()),
                started_on: started,
                finished_on: started + Duration::from_secs(1),
                artifacts: Vec::new(),
                annotations: serde_json::Map::new(),
            };
            v1::SignedEvidence {
                envelope: claim
                    .sign(&TreeDigest::git(&spec.tree_id).unwrap(), &key)
                    .unwrap()
                    .to_bytes(),
                public_key: key.public_key().spki().to_vec(),
            }
        })
        .collect();
    client
        .complete_job(v1::CompleteJobRequest {
            job_id: job.job_id,
            claim_token: claimed.claim_token,
            outcome: v1::JobOutcome::Done.into(),
            evidence,
            usage: None,
            message: String::new(),
        })
        .await
        .unwrap();
}

/// R3.05 test 14: `reef job submit --wait` follows a job an executor in
/// this process runs, printing its states on stderr, and exits 0 whatever
/// the results; `--for` gives up with exit 6 and leaves the job queued;
/// `list`, `list --queue`, `show`, `cancel` and `evidence list --job`.
/// Every output is checked against `reef schema` by `run`.
#[tokio::test(flavor = "multi_thread")]
async fn job_cli() {
    use reef_daemon::client::evidence::Outcome;

    let (daemon, repo) = fixture().await;
    let created = ok(&daemon, &repo.root, &["-o", "json", "ws", "create"]).await;
    let ws_dir = PathBuf::from(created["path"].as_str().unwrap());
    for (result, expected) in [(Outcome::Pass, "pass"), (Outcome::Fail, "fail")] {
        let serving = tokio::spawn(serve_one(daemon.client.clone(), result));
        let output = run_in(
            &daemon,
            &ws_dir,
            &[
                "-o",
                "json",
                "job",
                "submit",
                "--kind",
                "test",
                "--target",
                "a",
                "--duplicate",
                "--wait",
            ],
        )
        .await;
        serving.await.unwrap();
        let submitted = json(&output);
        assert_eq!(submitted["created"], true);
        let job = &submitted["job"];
        assert_eq!(job["state"], "completed", "{submitted}");
        assert_eq!(job["outcome"], "done");
        assert_eq!(job["coverage"][0]["result"], expected);
        assert_eq!(job["coverage"][0]["target"], "a");
        let err = stderr(&output);
        assert!(err.contains("queued"), "{err}");
        assert!(err.contains("completed (done)"), "{err}");
    }

    // No executor: --for gives up with exit 6, and the job stays queued.
    let output = run_in(
        &daemon,
        &ws_dir,
        &[
            "-o", "json", "job", "submit", "--kind", "lint", "--wait", "--for", "1s",
        ],
    )
    .await;
    output.clone().assert().code(6);
    let listed = ok(&daemon, &ws_dir, &["-o", "json", "job", "list", "--queue"]).await;
    let queued = listed.as_array().unwrap();
    assert_eq!(queued.len(), 1, "{listed}");
    assert_eq!(queued[0]["state"], "queued");
    let id = queued[0]["id"].as_str().unwrap().to_owned();

    // Text forms, a brief list, show and cancel by prefix.
    let text = run_in(&daemon, &ws_dir, &["-o", "text", "job", "list"]).await;
    assert!(
        stdout(&text).contains("completed (done)"),
        "{}",
        stdout(&text)
    );
    let brief = ok(&daemon, &ws_dir, &["job", "list"]).await;
    assert_eq!(brief.as_array().unwrap().len(), 3);
    assert!(brief[0].get("coverage").is_none(), "{brief}");
    let shown = run_in(&daemon, &ws_dir, &["-o", "text", "job", "show", &id[..10]]).await;
    assert!(stdout(&shown).contains("(none yet)"), "{}", stdout(&shown));
    let cancelled = ok(
        &daemon,
        &ws_dir,
        &["-o", "json", "job", "cancel", &id, "--reason", "not needed"],
    )
    .await;
    assert_eq!(cancelled["state"], "cancelled");
    assert_eq!(cancelled["message"], "not needed");
    let again = run_in(&daemon, &ws_dir, &["-o", "text", "job", "cancel", &id]).await;
    assert!(stdout(&again).contains("cancelled job"));

    // The evidence a job accepted.
    let completed: Vec<String> = ok(
        &daemon,
        &ws_dir,
        &["-o", "json", "job", "list", "--state", "completed"],
    )
    .await
    .as_array()
    .unwrap()
    .iter()
    .map(|job| job["id"].as_str().unwrap().to_owned())
    .collect();
    assert_eq!(completed.len(), 2);
    let evidence = ok(
        &daemon,
        &ws_dir,
        &["-o", "json", "evidence", "list", "--job", &completed[0]],
    )
    .await;
    assert_eq!(evidence.as_array().unwrap().len(), 1, "{evidence}");
    err(
        &daemon,
        &ws_dir,
        &["-o", "json", "job", "show", "job_0123456789abcdef"],
        3,
        "JOB_NOT_FOUND",
    )
    .await;
    daemon.stop().await;
}

/// R3.06 test 13: `reef executor check` and `run` through the CLI.
/// `check` refuses a sandbox this platform lacks with exit 6 and `next`,
/// a bad configuration with exit 2 naming its line, and prints the plan
/// at a revision; `run --once` runs a job and prints its line, or exits 0
/// (3 with --require-job) when none comes. Outputs are checked against
/// `reef schema` by `run`. The sandbox's own probes are test 9 below.
#[expect(clippy::too_many_lines, reason = "each refusal, then a plan and a job")]
#[tokio::test(flavor = "multi_thread")]
async fn executor_cli() {
    let (daemon, repo) = fixture().await;
    let created = ok(&daemon, &repo.root, &["-o", "json", "ws", "create"]).await;
    let ws_dir = PathBuf::from(created["path"].as_str().unwrap());
    std::fs::write(
        ws_dir.join("reef-checks.toml"),
        "[[check]]\nname = \"exit\"\nkind = \"test\"\ntargets = [\"*\"]\nrun_target = [\"sh\", \"-c\", \"exit \\\"$1\\\"\", \"sh\", \"{target}\"]\n",
    )
    .unwrap();
    let work = tempfile::tempdir().unwrap();
    let config = work.path().join("executor.toml");
    let other = if cfg!(target_os = "macos") {
        "bubblewrap"
    } else {
        "seatbelt"
    };
    std::fs::write(&config, format!("[sandbox]\nkind = \"{other}\"\n")).unwrap();
    let config_arg = config.display().to_string();

    // A sandbox this platform lacks: exit 6, the probe's reason, `next`.
    let output = run_in(
        &daemon,
        &ws_dir,
        &["-o", "json", "executor", "check", "--config", &config_arg],
    )
    .await;
    output.clone().assert().code(6);
    let error: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert!(
        error["error"]["message"].as_str().unwrap().contains("only"),
        "{error}"
    );
    assert_eq!(next_of(&error["error"]), ["reef", "executor", "check"]);
    // A stream's error is one line: the piped (brief) form.
    let output = run_in(
        &daemon,
        &ws_dir,
        &["executor", "run", "--once", "--config", &config_arg],
    )
    .await;
    output.clone().assert().code(6);

    // A bad configuration: exit 2, naming its line.
    std::fs::write(&config, "[executor]\nlease = \"1s\"\nnope = 1\n").unwrap();
    let output = run_in(
        &daemon,
        &ws_dir,
        &["-o", "json", "executor", "check", "--config", &config_arg],
    )
    .await;
    output.clone().assert().code(2);
    assert!(stdout(&output).contains("line 3"), "{}", stdout(&output));

    // No sandbox, by configuration: the plan at a revision, and a job.
    std::fs::write(
        &config,
        format!(
            "[work]\ndir = {:?}\n[sandbox]\nkind = \"none\"\n",
            work.path().join("work").display().to_string()
        ),
    )
    .unwrap();
    let checked = ok(
        &daemon,
        &ws_dir,
        &[
            "-o",
            "json",
            "executor",
            "check",
            "--config",
            &config_arg,
            "-r",
            "@",
            "--kind",
            "test",
            "--target",
            "0",
            "--target",
            "$(id)",
        ],
    )
    .await;
    assert_eq!(checked["sandbox"]["kind"], "none");
    let plan = checked["plan"].as_array().unwrap();
    assert_eq!(plan[0]["checks"][0]["argv"][4], "0", "{checked}");
    assert_eq!(plan[0]["checks"][0]["source"], "repository");
    assert_eq!(plan[1]["checks"], serde_json::json!([]));
    assert!(plan[1]["note"].as_str().unwrap().contains("target pattern"));
    let text = run_in(
        &daemon,
        &ws_dir,
        &[
            "-o",
            "text",
            "executor",
            "check",
            "--config",
            &config_arg,
            "-r",
            "@",
            "--kind",
            "test",
            "--target",
            "1",
        ],
    )
    .await;
    assert!(
        stdout(&text).contains("test 1: exit (repository"),
        "{}",
        stdout(&text)
    );

    // run --once with no job: 0, or 3 with --require-job.
    let idle = run_in(
        &daemon,
        &ws_dir,
        &[
            "-o",
            "json",
            "executor",
            "run",
            "--config",
            &config_arg,
            "--once",
            "--wait",
            "1s",
        ],
    )
    .await;
    idle.clone().assert().code(0);
    assert_eq!(stdout(&idle), "");
    let required = run_in(
        &daemon,
        &ws_dir,
        &[
            "executor",
            "run",
            "--config",
            &config_arg,
            "--once",
            "--wait",
            "1s",
            "--require-job",
        ],
    )
    .await;
    required.clone().assert().code(3);

    // A job: one JSON line.
    let submitted = ok(
        &daemon,
        &ws_dir,
        &[
            "-o", "json", "job", "submit", "--kind", "test", "--target", "0", "--target", "1",
        ],
    )
    .await;
    let lines = json_lines(
        &run_in(
            &daemon,
            &ws_dir,
            &[
                "-o",
                "json",
                "executor",
                "run",
                "--config",
                &config_arg,
                "--once",
                "--wait",
                "10s",
            ],
        )
        .await,
    );
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["job_id"], submitted["job"]["id"]);
    assert_eq!(lines[0]["outcome"], "done");
    assert_eq!(
        (lines[0]["pass"].as_u64(), lines[0]["fail"].as_u64()),
        (Some(1), Some(1))
    );
    assert_eq!(lines[0]["sandbox"], "none");

    // Listed in --help-all, not in the root help.
    let all = stdout(&run_in(&daemon, &ws_dir, &["--help-all"]).await);
    assert!(all.contains("executor"), "{all}");
    daemon.stop().await;
}

/// R3.06 test 9: `reef executor check` runs every escape probe inside this
/// platform's sandbox and a control run without it; each is refused inside
/// and allowed outside. Skipped (printed) where the sandbox cannot apply,
/// such as a runner already inside Seatbelt, unless
/// `REEF_EXECUTOR_TESTS_REQUIRE_SANDBOX` is set.
#[tokio::test(flavor = "multi_thread")]
async fn executor_sandbox_probes() {
    let (daemon, repo) = fixture().await;
    let output = run_in(&daemon, &repo.root, &["-o", "json", "executor", "check"]).await;
    let report: Value = serde_json::from_str(&stdout(&output)).unwrap();
    if !output.status.success() {
        let problem = report["error"]["message"].as_str().unwrap_or_default();
        assert!(
            std::env::var_os("REEF_EXECUTOR_TESTS_REQUIRE_SANDBOX").is_none(),
            "the sandbox must apply and hold here: {problem}"
        );
        assert!(
            problem.contains("cannot be applied") || problem.contains("Seatbelt"),
            "{report}"
        );
        eprintln!("skipped: the sandbox does not apply here: {problem}");
        output.clone().assert().code(6);
        daemon.stop().await;
        return;
    }
    assert_eq!(report["sandbox"]["holds"], true, "{report:#}");
    let probes = report["sandbox"]["probes"].as_array().unwrap();
    assert!(probes.len() >= 14, "{report:#}");
    for probe in probes {
        let detail = probe["detail"].as_str().unwrap();
        if probe["boundary"] == true {
            assert_eq!(probe["refused"], true, "{probe}");
        }
        // The control run reached what the sandbox refused.
        let control = detail.split("without the sandbox: ").nth(1).unwrap();
        assert!(
            !control.contains("denied")
                && !control.contains("not permitted")
                && !control.starts_with("did not run"),
            "{probe}"
        );
    }
    daemon.stop().await;
}

/// R3.07 test 15: `reef policy check`, `eval` and `init`.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "the three commands end to end")]
async fn policy_cli() {
    // `check` needs no daemon: a state directory nobody serves.
    let lone = tempfile::tempdir().unwrap();
    let no_daemon = |dir: &Path| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
        command
            .env_remove("REEF_SOCKET")
            .env_remove("REEF_OUTPUT")
            .env("REEF_STATE_DIR", lone.path().join("state"))
            .current_dir(dir)
            .arg("--no-autostart");
        command
    };
    let work = tempfile::tempdir().unwrap();
    std::fs::write(
        work.path().join("reef-policy.toml"),
        reef_policy::starter("alice"),
    )
    .unwrap();
    let checked = json(&run(no_daemon(work.path()), &["-o", "json", "policy", "check"]).await);
    assert_eq!(checked["version"], 1);
    assert_eq!(checked["rules"][4], "tests");
    std::fs::write(
        work.path().join("bad.toml"),
        "version = 1\n[owners]\ngroups = [\"x\"]\n",
    )
    .unwrap();
    let output = run(
        no_daemon(work.path()),
        &["-o", "json", "policy", "check", "bad.toml"],
    )
    .await;
    let error = failed(&output, 2, "INVALID_POLICY");
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .starts_with("bad.toml:3:11: owners.groups:"),
        "{error}"
    );
    // `--input` evaluates offline; a conflict input prints the tier.
    std::fs::write(
        work.path().join("in.json"),
        r#"{"change": {"paths": ["src/a.rs"]}}"#,
    )
    .unwrap();
    let evaluated = json(
        &run(
            no_daemon(work.path()),
            &["-o", "json", "policy", "check", "--input", "in.json"],
        )
        .await,
    );
    assert_eq!(evaluated["evaluation"]["decision"], "block");
    std::fs::write(
        work.path().join("conflict.json"),
        r#"{"conflict": {"paths": ["docs/a.md"]}}"#,
    )
    .unwrap();
    let tier = json(
        &run(
            no_daemon(work.path()),
            &["-o", "json", "policy", "check", "--input", "conflict.json"],
        )
        .await,
    );
    assert_eq!(tier["conflict"]["tier"], "approve");

    // Through the daemon.
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let dir = path(&ws);
    write(&ws, "reef-policy.toml", reef_policy::starter("alice"));
    let at = ok(&daemon, &dir, &["-o", "json", "policy", "check", "-r", "@"]).await;
    assert_eq!(at["path"], "@:reef-policy.toml");
    // No policy on trunk(): nothing lands automatically.
    let none = ok(&daemon, &dir, &["-o", "json", "policy", "eval"]).await;
    assert_eq!(none["decision"], "no_policy");
    // The workspace's own file, as text: blocked, `test` missing.
    let file = dir.join("reef-policy.toml");
    let file = file.to_str().unwrap();
    let blocked = ok(
        &daemon,
        &dir,
        &["-o", "json", "policy", "eval", "--policy-file", file],
    )
    .await;
    assert_eq!(blocked["decision"], "block", "{blocked}");
    assert_eq!(blocked["source"]["from_request"], true);
    let tests = blocked["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|rule| rule["name"] == "tests")
        .unwrap();
    assert_eq!(tests["missing"][0]["kind"], "test");
    let brief = ok(
        &daemon,
        &dir,
        &["-o", "brief", "policy", "eval", "--policy-file", file],
    )
    .await;
    // The change adds reef-policy.toml itself, so the owners must sign too.
    assert_eq!(brief["blocking"][0]["name"], "builtin:owners", "{brief}");
    assert_eq!(brief["blocking"][1]["name"], "tests", "{brief}");
    assert_eq!(brief["blocking"][1]["missing"][0], "test", "{brief}");
    run_in(&daemon, &dir, &["policy", "eval", "--policy-file", file]).await;
    let text = stdout(
        &run_in(
            &daemon,
            &dir,
            &["-o", "text", "policy", "eval", "--policy-file", file],
        )
        .await,
    );
    assert!(text.contains("decision: block"), "{text}");
    assert!(text.contains("missing: test from local"), "{text}");
    // --require-allow: exit 5, with the first unmet rule and the command.
    let output = run_in(
        &daemon,
        &dir,
        &[
            "-o",
            "json",
            "policy",
            "eval",
            "--policy-file",
            file,
            "--require-allow",
        ],
    )
    .await;
    let error = serde_json::json!({ "error": failed(&output, 5, "POLICY_BLOCKED") });
    assert!(
        error["error"]["hint"]
            .as_str()
            .unwrap()
            .starts_with("builtin:owners:"),
        "{error}"
    );
    let text = run_in(
        &daemon,
        &dir,
        &[
            "-o",
            "text",
            "policy",
            "eval",
            "--policy-file",
            file,
            "--require-allow",
        ],
    )
    .await;
    text.clone().assert().code(5);
    assert!(stdout(&text).contains("decision: block"));
    assert_eq!(
        strings(&error["error"]["next"]),
        ["reef", "policy", "eval", "-r", "@"]
    );
    // The input, printed and read back offline, evaluates the same.
    let with_input = ok(
        &daemon,
        &dir,
        &[
            "-o",
            "json",
            "policy",
            "eval",
            "--policy-file",
            file,
            "--input-json",
        ],
    )
    .await;
    std::fs::write(
        work.path().join("seen.json"),
        serde_json::to_string(&with_input["input_json"]).unwrap(),
    )
    .unwrap();
    let replayed = json(
        &run(
            no_daemon(&dir),
            &[
                "-o",
                "json",
                "policy",
                "check",
                "--input",
                work.path().join("seen.json").to_str().unwrap(),
            ],
        )
        .await,
    );
    assert_eq!(replayed["evaluation"]["rules"], with_input["rules"]);
    assert_eq!(replayed["evaluation"]["decision"], with_input["decision"]);

    // `init` names the default identity, refuses to overwrite, and prints
    // with --stdout.
    let fresh = tempfile::tempdir().unwrap();
    let init = ok(&daemon, fresh.path(), &["-o", "json", "policy", "init"]).await;
    assert_eq!(init["placeholder"], false);
    let owner = init["owner"].as_str().unwrap().to_owned();
    let written = std::fs::read_to_string(fresh.path().join("reef-policy.toml")).unwrap();
    assert!(written.contains(&format!("identities = [\"{owner}\"]")));
    ok(&daemon, fresh.path(), &["-o", "json", "policy", "check"]).await;
    err(
        &daemon,
        fresh.path(),
        &["-o", "json", "policy", "init"],
        4,
        "POLICY_EXISTS",
    )
    .await;
    let printed = stdout(&run_in(&daemon, fresh.path(), &["policy", "init", "--stdout"]).await);
    assert_eq!(printed, written);
    daemon.stop().await;
}
