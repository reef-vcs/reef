//! R3.12 test 15: `reef comment next --hook`, run as the plugin's
//! instruction hooks run it, against a daemon: it takes the session's
//! instruction, prints it framed in the hook's form, declares its hash on
//! the session's workspaces, and otherwise prints nothing and never fails.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use reef_api::v1::{self, CreateWorkspaceRequest, GetWorkspaceRequest};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_testing::synth::Preset;
use serde_json::{Value, json};

/// The plugin's turn-end hook.
const STOP: &[&str] = &[
    "comment",
    "next",
    "--hook",
    "--at",
    "client_session=/session_id",
    "--at",
    "workspace=/cwd",
    "--emit",
    "block",
];

/// The plugin's prompt hook.
const PROMPT: &[&str] = &[
    "comment",
    "next",
    "--hook",
    "--at",
    "client_session=/session_id",
    "--at",
    "workspace=/cwd",
    "--emit",
    "context",
];

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
    })
    .await
}

/// `reef` with `state` as its state directory, from `dir`, with none of
/// the machine's harness in its environment.
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
        "REEF_CLIENT_SESSION",
        "AI_AGENT",
        "CLAUDECODE",
        "CLAUDE_CODE_SESSION_ID",
        "OPENCODE",
    ] {
        command.env_remove(var);
    }
    command.env("REEF_STATE_DIR", state).current_dir(dir);
    command
}

async fn run(mut command: Command, args: &[&str], stdin: String) -> Output {
    command.args(args).write_stdin(stdin.into_bytes());
    let started = Instant::now();
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stderr), "", "{args:?}");
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{args:?} took {:?}",
        started.elapsed()
    );
    output
}

fn event(session: &str, cwd: &Path, name: &str) -> String {
    json!({
        "session_id": session,
        "transcript_path": "/dev/null",
        "cwd": cwd,
        "hook_event_name": name,
    })
    .to_string()
}

#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "two instructions through both hooks")]
async fn hook_hands_the_session_its_instruction() {
    let (daemon, repo) = fixture().await;
    let state = daemon.state_dir();
    let workspace = daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "test@host".to_owned(),
            provenance: Some(v1::ProvenanceDeclaration {
                client_session: Some("s-hook".to_owned()),
                ..v1::ProvenanceDeclaration::default()
            }),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    let dir = Path::new(&workspace.path).to_path_buf();
    std::fs::write(dir.join("h.txt"), "h\n").unwrap();
    daemon
        .client
        .snapshot_workspace("", &workspace.id, "")
        .await
        .unwrap();
    let mut ids = Vec::new();
    for text in ["first: rename h", "second: add a test"] {
        let written = daemon
            .client
            .create_comment(v1::CreateCommentRequest {
                workspace: workspace.id.clone(),
                revision: "@".to_owned(),
                kind: v1::CommentKind::Instruction.into(),
                text: text.to_owned(),
                confirm: true,
                ceilings: Some(v1::InstructionCeilings {
                    max_check_wall_ms: 1,
                    allow_unknown: true,
                    ..v1::InstructionCeilings::default()
                }),
                ..v1::CreateCommentRequest::default()
            })
            .await
            .unwrap()
            .comment
            .unwrap();
        ids.push((written.id, written.text_sha256));
    }

    // Another session's hook prints nothing.
    let output = run(
        reef(state, &repo.root),
        PROMPT,
        event("s-else", &repo.root, "UserPromptSubmit"),
    )
    .await;
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");

    // The turn-end hook keeps the agent working with the first.
    let output = run(reef(state, &repo.root), STOP, event("s-hook", &dir, "Stop")).await;
    let decision: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(decision["decision"], "block");
    let reason = decision["reason"].as_str().unwrap();
    assert!(
        reason.starts_with(&format!("Reef instruction {}", ids[0].0)),
        "{reason}"
    );
    assert!(reason.contains("first: rename h"), "{reason}");
    // The prompt hook adds the second as plain context.
    let output = run(
        reef(state, &repo.root),
        PROMPT,
        event("s-hook", &dir, "UserPromptSubmit"),
    )
    .await;
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("second: add a test"), "{text}");
    // Nothing left: nothing printed.
    let output = run(reef(state, &repo.root), STOP, event("s-hook", &dir, "Stop")).await;
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");

    let read = daemon
        .client
        .get_comment(v1::GetCommentRequest {
            repo: repo.id.clone(),
            id: ids[0].0.clone(),
        })
        .await
        .unwrap()
        .comment
        .unwrap();
    let instruction = read.instruction.unwrap();
    assert_eq!(instruction.state(), v1::InstructionState::Delivered);
    assert_eq!(instruction.channel(), v1::DeliveryChannel::Hook);
    assert_eq!(instruction.claimed_client_session, "s-hook");
    // The session's workspace declares the last instruction's hash.
    let context = daemon
        .client
        .get_workspace(GetWorkspaceRequest {
            workspace: workspace.id.clone(),
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
        .provenance
        .unwrap()
        .context
        .unwrap();
    assert_eq!(context.instruction_sha256, ids[1].1);
}

#[tokio::test(flavor = "multi_thread")]
async fn hook_never_fails_and_starts_no_daemon() {
    let empty = tempfile::tempdir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    for stdin in [
        event("s", dir.path(), "Stop"),
        "not json".to_owned(),
        json!({"session_id": 3}).to_string(),
    ] {
        let output = run(reef(empty.path(), dir.path()), STOP, stdin).await;
        assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    }
    assert!(
        !empty.path().join("daemon.sock").exists(),
        "a daemon was started"
    );
}
