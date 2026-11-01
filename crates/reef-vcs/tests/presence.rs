//! R2.25: `reef presence hook`, run as the Claude Code plugin's hooks run
//! it, against a daemon. Tests are numbered as the brief numbers them.
//!
//! The event is `fixtures/hooks/claude-code-pre-edit.json`, a `PreToolUse`
//! event in Claude Code's documented shape, with its session, directory
//! and path filled in by each test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use reef_api::v1::{self, CreateWorkspaceRequest};
use reef_daemon::testing::{SyntheticRepo, TestDaemon};
use reef_testing::synth::Preset;
use serde_json::Value;

const EVENT: &str = include_str!("fixtures/hooks/claude-code-pre-edit.json");

/// The plugin's hook arguments (`plugins/claude-code/hooks/hooks.json`).
const EDIT: &[&str] = &[
    "presence",
    "hook",
    "edit",
    "--path-at",
    "/tool_input/file_path",
    "--path-at",
    "/tool_input/notebook_path",
    "--session-at",
    "/session_id",
];
const END: &[&str] = &["presence", "hook", "end", "--session-at", "/session_id"];

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
    })
    .await
}

async fn create(daemon: &TestDaemon, repo: &SyntheticRepo) -> v1::Workspace {
    daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "test@host".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// The fixture event for `session`, editing `file`, run from `cwd`.
fn event(session: &str, cwd: &Path, file: &str) -> String {
    EVENT
        .replace("__SESSION__", session)
        .replace("__CWD__", &cwd.display().to_string())
        .replace("__FILE__", file)
}

/// `reef` with `state` as its state directory, from `dir`, with a clean
/// environment and no `--no-autostart`: the hook implies it.
fn reef(state: &Path, dir: &Path) -> Command {
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
    command.env("REEF_STATE_DIR", state).current_dir(dir);
    command
}

/// Runs `command` with `args` and `stdin` off the runtime's worker threads,
/// checking it as `reef schema` describes it.
async fn run(mut command: Command, args: &[&str], stdin: impl Into<Vec<u8>>) -> Output {
    command.args(args).write_stdin(stdin.into());
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    common::schema::check(args, &output);
    output
}

/// A hook run: exits 0 and prints nothing, whatever happened.
async fn hook(command: Command, args: &[&str], stdin: impl Into<Vec<u8>>) {
    let started = Instant::now();
    let output = run(command, args, stdin).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "", "{args:?}");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{args:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{args:?} took {:?}",
        started.elapsed()
    );
}

/// The live editing entries declared by clients (not the daemon's).
async fn declared(daemon: &TestDaemon, dir: &Path) -> Vec<Value> {
    let mut command = reef(daemon.state_dir(), dir);
    command.arg("--no-autostart");
    let args = [
        "presence", "watch", "--for", "0", "--kind", "editing", "-o", "json",
    ];
    let output = run(command, &args, Vec::new()).await;
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .map(|event| event["presence"].clone())
        .filter(|presence| presence["by_daemon"] == false)
        .collect()
}

fn hooks_dir(state: &Path) -> PathBuf {
    state.join("presence/hooks")
}

// R2.25 9: the plugin's edit hook declares the file as an editing entry,
// one per session, replaced by the session's next edit and cleared at its
// end.
#[tokio::test(flavor = "multi_thread")]
async fn hook_reports_declared_edits() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let root = PathBuf::from(&ws.path);
    let state = daemon.state_dir();
    let first = root.join("src/a.py");
    let in_ws = || reef(state, &root);

    hook(
        in_ws(),
        EDIT,
        event("s-1", &root, &first.display().to_string()),
    )
    .await;
    let entries = declared(&daemon, &root).await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["kind"], "editing");
    assert_eq!(entries[0]["paths"], serde_json::json!(["src/a.py"]));
    assert_eq!(entries[0]["workspace_id"], ws.id.as_str());
    assert_eq!(entries[0]["by_daemon"], false);
    let id = entries[0]["id"].clone();

    // A relative path, against the hook's directory, replaces the entry.
    hook(in_ws(), EDIT, event("s-1", &root, "docs/b.md")).await;
    let entries = declared(&daemon, &root).await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["id"], id);
    assert_eq!(entries[0]["paths"], serde_json::json!(["docs/b.md"]));

    // Another session is another entry; a hook run from the root checkout
    // still finds the workspace by the path.
    hook(
        reef(state, &repo.root),
        EDIT,
        event("s-2", &repo.root, &first.display().to_string()),
    )
    .await;
    assert_eq!(declared(&daemon, &root).await.len(), 2);

    // A notebook's path is at another pointer.
    let notebook: Value = serde_json::json!({
        "session_id": "s-3",
        "hook_event_name": "PreToolUse",
        "tool_name": "NotebookEdit",
        "tool_input": {"notebook_path": root.join("n.ipynb"), "new_source": "x"},
    });
    hook(in_ws(), EDIT, notebook.to_string()).await;
    let entries = declared(&daemon, &root).await;
    assert!(
        entries
            .iter()
            .any(|entry| entry["paths"] == serde_json::json!(["n.ipynb"])),
        "{entries:?}"
    );

    // A write into a directory that does not exist yet still resolves.
    hook(
        in_ws(),
        EDIT,
        event(
            "s-4",
            &root,
            &root.join("new/dir/c.rs").display().to_string(),
        ),
    )
    .await;
    assert!(
        declared(&daemon, &root)
            .await
            .iter()
            .any(|entry| entry["paths"] == serde_json::json!(["new/dir/c.rs"])),
        "a new directory's file was not declared"
    );

    // The end hook clears the session's entry and its id file.
    let end = serde_json::json!({"session_id": "s-1", "hook_event_name": "SessionEnd"});
    assert!(hooks_dir(state).join("s-1.id").exists());
    hook(in_ws(), END, end.to_string()).await;
    let entries = declared(&daemon, &root).await;
    assert!(entries.iter().all(|entry| entry["id"] != id), "{entries:?}");
    assert_eq!(entries.len(), 3, "{entries:?}");
    assert!(!hooks_dir(state).join("s-1.id").exists());
    // Ending again finds nothing, and is still silent.
    hook(in_ws(), END, end.to_string()).await;
}

// R2.25 10: whatever goes wrong, the hook exits 0, prints nothing, and
// returns within three seconds.
#[tokio::test(flavor = "multi_thread")]
async fn hook_never_fails() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let other = create(&daemon, &repo).await;
    let root = PathBuf::from(&ws.path);
    let state = daemon.state_dir();
    let file = root.join("a.txt").display().to_string();

    // No daemon: none is started.
    let empty = tempfile::tempdir().unwrap();
    hook(reef(empty.path(), &root), EDIT, event("s", &root, &file)).await;
    assert!(
        !empty.path().join("daemon.sock").exists()
            && std::fs::read_dir(empty.path())
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| entry.file_name() != "daemon.pid"),
        "a daemon was started"
    );

    // Bad input.
    hook(reef(state, &root), EDIT, "not json").await;
    hook(reef(state, &root), EDIT, vec![b' '; (1 << 20) + 1]).await;
    hook(reef(state, &root), END, "[").await;
    // Pointers that select nothing, a path in no workspace, and paths in
    // two workspaces.
    hook(reef(state, &root), EDIT, r#"{"session_id": "s"}"#).await;
    let outside = tempfile::tempdir().unwrap();
    let nowhere = outside.path().join("x.txt").display().to_string();
    hook(reef(state, &root), EDIT, event("s", &root, &nowhere)).await;
    let two = serde_json::json!({
        "session_id": "s",
        "tool_input": {"file_path": file, "notebook_path": PathBuf::from(&other.path).join("b.txt")},
    });
    hook(reef(state, &root), EDIT, two.to_string()).await;
    assert_eq!(declared(&daemon, &root).await, Vec::<Value>::new());

    // A daemon that accepts and never answers: the deadline ends it.
    let silent = tempfile::tempdir().unwrap();
    let socket = silent.path().join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let held = std::thread::spawn(move || {
        let held: Vec<_> = listener.incoming().take(4).collect();
        std::thread::sleep(Duration::from_secs(5));
        drop(held);
    });
    let mut command = reef(silent.path(), &root);
    command.env("REEF_SOCKET", &socket);
    hook(command, EDIT, event("s", &root, &file)).await;
    drop(held);
}

// R2.25 9, a session that moves: its next edit in another workspace ends
// the old entry and starts one there.
#[tokio::test(flavor = "multi_thread")]
async fn hook_follows_a_session_to_another_workspace() {
    let (daemon, repo) = fixture().await;
    let first = create(&daemon, &repo).await;
    let second = create(&daemon, &repo).await;
    let state = daemon.state_dir();
    let (a, b) = (PathBuf::from(&first.path), PathBuf::from(&second.path));
    hook(
        reef(state, &a),
        EDIT,
        event("s", &a, &a.join("x.txt").display().to_string()),
    )
    .await;
    hook(
        reef(state, &b),
        EDIT,
        event("s", &b, &b.join("y.txt").display().to_string()),
    )
    .await;
    let entries = declared(&daemon, &a).await;
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!(entries[0]["workspace_id"], second.id.as_str());
    assert_eq!(entries[0]["paths"], serde_json::json!(["y.txt"]));
}
