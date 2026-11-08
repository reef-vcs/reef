//! R3.44: `reef provenance declare --hook`, run as the Claude Code
//! plugin's prompt hook runs it, against a daemon; and the workspaces a
//! harness session creates declaring from its environment and its note.
//! Tests are numbered as the brief numbers them.

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
use reef_api::v1::{self, CreateWorkspaceRequest, GetWorkspaceRequest, ListProvenanceRequest};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_testing::synth::Preset;
use serde_json::{Value, json};

/// The plugin's prompt hook (`plugins/claude-code/hooks/hooks.json`).
const PROMPT: &[&str] = &[
    "provenance",
    "declare",
    "--hook",
    "--at",
    "instruction=/prompt",
    "--at",
    "client_session=/session_id",
    "--at",
    "workspace=/cwd",
];

/// The plugin's session-start hook.
const START: &[&str] = &[
    "provenance",
    "declare",
    "--hook",
    "--at",
    "model=/model",
    "--at",
    "client_session=/session_id",
    "--at",
    "workspace=/cwd",
];

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
    })
    .await
}

/// `reef` with `state` as its state directory, from `dir`, with a clean
/// environment: none of the machine's harness, and Claude Code's signals
/// when `session` names one.
fn reef(state: &Path, dir: &Path, session: Option<&str>) -> Command {
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
        "REEF_MODEL",
        "REEF_TASK",
        "AI_AGENT",
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "OPENCODE",
    ] {
        command.env_remove(var);
    }
    command.env("REEF_STATE_DIR", state).current_dir(dir);
    if let Some(session) = session {
        command
            .env("CLAUDECODE", "1")
            .env("CLAUDE_CODE_SESSION_ID", session)
            .env("AI_AGENT", "claude-code_2-1-316_agent");
    }
    command
}

async fn run(mut command: Command, args: &[&str], stdin: impl Into<Vec<u8>>) -> Output {
    command.args(args).write_stdin(stdin.into());
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    common::schema::check(args, &output);
    output
}

/// A hook run: exits 0 and prints nothing, within the deadline.
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

/// A `UserPromptSubmit` event in Claude Code's documented shape.
fn prompt_event(session: &str, cwd: &Path, prompt: &str) -> String {
    json!({
        "session_id": session,
        "transcript_path": "/dev/null",
        "cwd": cwd,
        "permission_mode": "default",
        "hook_event_name": "UserPromptSubmit",
        "prompt": prompt,
    })
    .to_string()
}

fn sha256(text: &str) -> String {
    reef_client::evidence::sha256_hex(text.as_bytes())
}

/// A prompt of about 2 KiB that appears nowhere else.
fn long_prompt(tag: &str) -> String {
    format!("{tag}: {}\nfix the parser\n", "lorem ipsum ".repeat(170))
}

async fn context(daemon: &TestDaemon, id: &str) -> v1::ProvenanceContext {
    daemon
        .client
        .get_workspace(GetWorkspaceRequest {
            workspace: id.to_owned(),
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
        .provenance
        .unwrap()
        .context
        .unwrap()
}

async fn records(daemon: &TestDaemon, repo: &SyntheticRepo, ws: &str) -> Vec<v1::ProvenanceRecord> {
    daemon
        .client
        .list_provenance_page(ListProvenanceRequest {
            repo: repo.id.clone(),
            workspace_id: ws.to_owned(),
            ..ListProvenanceRequest::default()
        })
        .await
        .unwrap()
        .records
}

/// `reef ws create` as a harness session's agent runs it.
async fn create_in_session(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    session: &str,
) -> v1::Workspace {
    let output = run(
        reef(daemon.state_dir(), &repo.root, Some(session)),
        &["ws", "create", "-o", "json", "--owner", "test@host"],
        Vec::new(),
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let created: Value = serde_json::from_slice(&output.stdout).unwrap();
    let id = created["id"].as_str().unwrap();
    daemon
        .client
        .get_workspace(GetWorkspaceRequest {
            workspace: id.to_owned(),
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

// R3.44 6: the prompt hook declares the instruction's hash, the session
// and the harness for the workspace it runs in, after recording the edit
// before it under the old context.
#[tokio::test(flavor = "multi_thread")]
async fn prompt_hook_declares_in_its_workspace() {
    let (daemon, repo) = fixture().await;
    let ws = daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "test@host".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    let root = PathBuf::from(&ws.path);
    let state = daemon.state_dir();
    std::fs::write(root.join("before.txt"), "before\n").unwrap();
    let prompt = long_prompt("first");
    hook(
        reef(state, &root, Some("s-6")),
        PROMPT,
        prompt_event("s-6", &root, &prompt),
    )
    .await;
    let declared = context(&daemon, &ws.id).await;
    assert_eq!(declared.instruction_sha256, sha256(&prompt));
    assert_eq!(declared.client_session, "s-6");
    assert_eq!(declared.harness, "claude-code/2.1.316");
    // The edit before the prompt was recorded under no instruction.
    let before = records(&daemon, &repo, &ws.id).await;
    assert_eq!(before.len(), 1, "{before:?}");
    assert_eq!(
        before[0]
            .context
            .clone()
            .unwrap_or_default()
            .instruction_sha256,
        ""
    );
    // The session start's model.
    let start = json!({"session_id": "s-6", "cwd": root, "hook_event_name": "SessionStart", "source": "startup", "model": "example-model-1"});
    hook(reef(state, &root, Some("s-6")), START, start.to_string()).await;
    assert_eq!(context(&daemon, &ws.id).await.model, "example-model-1");
    // A subdirectory is in the workspace too.
    let sub = root.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    let next = long_prompt("second");
    hook(
        reef(state, &sub, Some("s-6")),
        PROMPT,
        prompt_event("s-6", &sub, &next),
    )
    .await;
    assert_eq!(
        context(&daemon, &ws.id).await.instruction_sha256,
        sha256(&next)
    );
}

// R3.44 6: whatever goes wrong, the hook exits 0, prints nothing and
// returns within the deadline; it starts no daemon.
#[tokio::test(flavor = "multi_thread")]
async fn prompt_hook_never_fails() {
    let (daemon, repo) = fixture().await;
    let root = repo.root.clone();
    let state = daemon.state_dir();
    let empty = tempfile::tempdir().unwrap();
    hook(
        reef(empty.path(), &root, Some("s")),
        PROMPT,
        prompt_event("s", &root, "hello"),
    )
    .await;
    assert!(
        !empty.path().join("daemon.sock").exists(),
        "a daemon was started"
    );
    // The note is written without a daemon, for the creations to come.
    assert!(empty.path().join("provenance/sessions/s.json").exists());
    for stdin in [
        "not json".to_owned(),
        "[1]".to_owned(),
        String::from_utf8(vec![b' '; (1 << 20) + 1]).unwrap(),
        json!({"prompt": 3, "session_id": "s"}).to_string(),
        json!({"session_id": "two words", "prompt": "x", "cwd": "/nowhere"}).to_string(),
    ] {
        hook(reef(state, &root, None), PROMPT, stdin).await;
    }
    // A field it does not take is the plugin's bug, and still not the
    // prompt's failure.
    hook(
        reef(state, &root, None),
        &["provenance", "declare", "--hook", "--at", "nope=/x"],
        "{}",
    )
    .await;
    // A released workspace.
    let ws = create_in_session(&daemon, &repo, "s-ended").await;
    daemon
        .client
        .release_workspace(&repo.id, &ws.id, false, false)
        .await
        .unwrap();
    hook(
        reef(state, &root, Some("s-ended")),
        PROMPT,
        prompt_event("s-ended", Path::new(&ws.path), "x"),
    )
    .await;
    // A daemon that accepts and never answers: the deadline ends it.
    let silent = tempfile::tempdir().unwrap();
    let socket = silent.path().join("daemon.sock");
    let listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    let held = std::thread::spawn(move || {
        let held: Vec<_> = listener.incoming().take(4).collect();
        std::thread::sleep(Duration::from_secs(5));
        drop(held);
    });
    let mut command = reef(silent.path(), &root, Some("s"));
    command.env("REEF_SOCKET", &socket);
    hook(command, PROMPT, prompt_event("s", &root, "hello")).await;
    drop(held);
}

// R3.44 7: from the root checkout, the prompt reaches every active
// workspace of its session and no other, and the workspace the session
// creates next is created under it.
#[tokio::test(flavor = "multi_thread")]
async fn prompt_reaches_the_sessions_workspaces() {
    let (daemon, repo) = fixture().await;
    let state = daemon.state_dir();
    let first = create_in_session(&daemon, &repo, "s-7").await;
    let second = create_in_session(&daemon, &repo, "s-7").await;
    let other = create_in_session(&daemon, &repo, "s-other").await;
    let created = context(&daemon, &first.id).await;
    assert_eq!(created.client_session, "s-7");
    assert_eq!(created.harness, "claude-code/2.1.316");
    let prompt = long_prompt("seven");
    hook(
        reef(state, &repo.root, Some("s-7")),
        PROMPT,
        prompt_event("s-7", &repo.root, &prompt),
    )
    .await;
    for ws in [&first, &second] {
        assert_eq!(
            context(&daemon, &ws.id).await.instruction_sha256,
            sha256(&prompt),
            "{}",
            ws.id
        );
    }
    assert_eq!(context(&daemon, &other.id).await.instruction_sha256, "");
    let next = create_in_session(&daemon, &repo, "s-7").await;
    let declared = context(&daemon, &next.id).await;
    assert_eq!(declared.instruction_sha256, sha256(&prompt));
    assert_eq!(declared.client_session, "s-7");
    // `REEF_MODEL` and a flag still win.
    let mut command = reef(state, &repo.root, Some("s-7"));
    command.env("REEF_MODEL", "env-model");
    let output = run(
        command,
        &[
            "ws",
            "create",
            "-o",
            "json",
            "--owner",
            "test@host",
            "--task",
            "T-7",
        ],
        Vec::new(),
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let created: Value = serde_json::from_slice(&output.stdout).unwrap();
    let declared = context(&daemon, created["id"].as_str().unwrap()).await;
    assert_eq!(declared.model, "env-model");
    assert_eq!(declared.task, "T-7");
    assert_eq!(declared.instruction_sha256, sha256(&prompt));
}

// R3.44 8: the prompt's text appears nowhere: not in the state directory
// (the database, the notes, the logs) nor in the CLI's own trace.
#[tokio::test(flavor = "multi_thread")]
async fn the_prompt_text_is_kept_nowhere() {
    let (daemon, repo) = fixture().await;
    let state = daemon.state_dir();
    let ws = create_in_session(&daemon, &repo, "s-8").await;
    let root = PathBuf::from(&ws.path);
    let marker = "zq-unique-prompt-marker-81f3";
    let prompt = format!("{marker} please refactor");
    let mut command = reef(state, &root, Some("s-8"));
    command.env("REEF_LOG", "trace");
    let output = run(command, PROMPT, prompt_event("s-8", &root, &prompt)).await;
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    let trace = String::from_utf8_lossy(&output.stderr);
    assert!(trace.contains("provenance hook"), "no trace: {trace}");
    assert!(!trace.contains(marker), "the trace holds the prompt");
    assert_eq!(
        context(&daemon, &ws.id).await.instruction_sha256,
        sha256(&prompt)
    );
    // Snapshot something under it, so a record is written too.
    std::fs::write(root.join("after.txt"), "after\n").unwrap();
    daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let mut dirs = vec![state.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap().flatten() {
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                // Workspaces hold the repository's files, not Reef's state.
                if path.file_name().is_some_and(|name| name == "ws") {
                    continue;
                }
                dirs.push(path);
            } else if kind.is_file()
                && let Ok(bytes) = std::fs::read(&path)
            {
                let found = bytes
                    .windows(marker.len())
                    .any(|window| window == marker.as_bytes());
                assert!(!found, "{} holds the prompt", path.display());
            }
        }
    }
}

// R3.44 4: `ws run` and `ws provider create` declare from the environment
// and the note too; plain `declare` reads none of it.
#[tokio::test(flavor = "multi_thread")]
async fn creations_declare_from_the_environment() {
    let (daemon, repo) = fixture().await;
    let state = daemon.state_dir();
    let prompt = long_prompt("four");
    hook(
        reef(state, &repo.root, Some("s-4")),
        PROMPT,
        prompt_event("s-4", &repo.root, &prompt),
    )
    .await;
    let output = run(
        reef(state, &repo.root, Some("s-4")),
        &["ws", "run", "--keep", "-o", "json", "--", "true"],
        Vec::new(),
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    // The report is the JSON value that ends stdout.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let start = stdout.rfind("\n{").map_or(0, |at| at + 1);
    let report: Value = serde_json::from_str(&stdout[start..]).unwrap();
    let ran = report["workspace_id"].as_str().unwrap().to_owned();
    let declared = context(&daemon, &ran).await;
    assert_eq!(declared.instruction_sha256, sha256(&prompt));
    assert_eq!(declared.client_session, "s-4");
    assert_eq!(declared.harness, "claude-code/2.1.316");
    let output = run(
        reef(state, &repo.root, Some("s-4")),
        &[
            "ws", "provider", "create", "--name", "r344-sub", "-o", "json",
        ],
        Vec::new(),
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let provided: Value = serde_json::from_slice(&output.stdout).unwrap();
    let declared = context(&daemon, provided["workspace_id"].as_str().unwrap()).await;
    assert_eq!(declared.instruction_sha256, sha256(&prompt));
    assert_eq!(declared.client_session, "s-4");
    // Plain `declare` takes only its flags: the environment's session and
    // harness, and the note, stay out.
    let plain = daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "test@host".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    let output = run(
        reef(state, Path::new(&plain.path), Some("s-4")),
        &["provenance", "declare", "--task", "T-plain", "-o", "json"],
        Vec::new(),
    )
    .await;
    assert!(output.status.success(), "{output:?}");
    let declared = context(&daemon, &plain.id).await;
    assert_eq!(declared.task, "T-plain");
    assert_eq!(declared.client_session, "");
    assert_eq!(declared.harness, "");
    assert_eq!(declared.instruction_sha256, "");
}

// R3.44: on a daemon that records no provenance, what the environment
// declares never fails a creation; a flag still does.
#[tokio::test(flavor = "multi_thread")]
async fn the_environment_never_fails_a_creation() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.provenance.enabled = false;
    })
    .await;
    let state = daemon.state_dir();
    for args in [
        &["ws", "create", "-o", "json", "--owner", "test@host"][..],
        &[
            "ws",
            "create",
            "-o",
            "json",
            "--owner",
            "test@host",
            "--model",
            "",
        ],
        &[
            "ws", "provider", "create", "--name", "r344-off", "-o", "json",
        ],
    ] {
        let mut command = reef(state, &repo.root, Some("s-off"));
        command.env("REEF_TASK", "T-env");
        let output = run(command, args, Vec::new()).await;
        assert!(output.status.success(), "{args:?}: {output:?}");
    }
    let output = run(
        reef(state, &repo.root, Some("s-off")),
        &[
            "ws",
            "create",
            "-o",
            "json",
            "--owner",
            "test@host",
            "--task",
            "T-1",
        ],
        Vec::new(),
    )
    .await;
    assert!(!output.status.success(), "{output:?}");
}
