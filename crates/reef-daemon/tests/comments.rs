//! Comments and instructions (R3.12, tests 3 to 13): notes and anchors,
//! the instruction's states, confirmation and its ceilings, the estimate,
//! routing and claims, the moves transitions and expiry make, start-up
//! resolution of pending rows, undo's refusal, and the remote listener's
//! rules.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, ClaimInstructionRequest, CommentAnchor, CommentKind, CreateCommentRequest,
    CreateIdentityRequest, CreateWorkspaceRequest, EditFilesRequest, FileEdit, GetCommentRequest,
    IdentityKind, InstructionCause, InstructionCeilings, InstructionState as S, LifecycleState,
    LineSide, ListCommentsRequest, ListOperationsRequest, OperationFilter, OperationKind,
    ProvenanceDeclaration, RemoteScope, TargetSource, TransitionChangeRequest,
    UndoOperationRequest, UpdateInstructionRequest, Workspace, edit_files_request, file_edit,
};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_testing::synth::Preset;
use tonic::Code;

const OWNER: &str = "tester@host";

fn remote_on(config: &mut Config) {
    config.file.remote.enabled = true;
    "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
    config.file.remote.names = vec!["localhost".to_owned()];
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        remote_on(config);
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

fn reason(error: &ClientError) -> (Code, String) {
    match error {
        ClientError::Rpc { status, reason, .. } => (status.code(), reason.clone()),
        other => panic!("{other:?}"),
    }
}

/// A new commit writing `files`, on `on` (a new change) or rewriting
/// `revision`.
async fn edit(
    client: &Client,
    repo: &str,
    target: edit_files_request::Target,
    files: &[(&str, Option<&str>)],
) -> v1::Change {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(target),
            edits: files
                .iter()
                .map(|(path, text)| FileEdit {
                    path: (*path).to_owned(),
                    action: Some(match text {
                        Some(text) => file_edit::Action::Content(text.as_bytes().to_vec()),
                        None => file_edit::Action::Delete(v1::FileDelete {}),
                    }),
                    executable: None,
                })
                .collect(),
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

async fn commit_on(client: &Client, repo: &str, on: &str, files: &[(&str, &str)]) -> v1::Change {
    let files: Vec<(&str, Option<&str>)> = files.iter().map(|(p, t)| (*p, Some(*t))).collect();
    edit(
        client,
        repo,
        edit_files_request::Target::NewOn(on.to_owned()),
        &files,
    )
    .await
}

/// A base commit, and a change on it that writes `a.txt` (three lines) and
/// deletes `README.md`.
async fn setup(client: &Client, repo: &str) -> (v1::Change, v1::Change) {
    let base = commit_on(
        client,
        repo,
        "root()",
        &[("README.md", "base\n"), ("keep.txt", "k\n")],
    )
    .await;
    let change = edit(
        client,
        repo,
        edit_files_request::Target::NewOn(base.commit_id.clone()),
        &[("a.txt", Some("one\ntwo\nthree\n")), ("README.md", None)],
    )
    .await;
    (base, change)
}

fn note(revision: &str, text: &str) -> CreateCommentRequest {
    CreateCommentRequest {
        revision: revision.to_owned(),
        kind: CommentKind::Note.into(),
        text: text.to_owned(),
        ..CreateCommentRequest::default()
    }
}

fn instruction(revision: &str, text: &str) -> CreateCommentRequest {
    CreateCommentRequest {
        kind: CommentKind::Instruction.into(),
        ..note(revision, text)
    }
}

async fn create(
    client: &Client,
    repo: &str,
    request: CreateCommentRequest,
) -> Result<v1::CreateCommentResponse, ClientError> {
    client
        .create_comment(CreateCommentRequest {
            repo: repo.to_owned(),
            ..request
        })
        .await
}

async fn move_to(
    client: &Client,
    repo: &str,
    id: &str,
    to: S,
    summary: &str,
) -> Result<v1::UpdateInstructionResponse, ClientError> {
    client
        .update_instruction(UpdateInstructionRequest {
            repo: repo.to_owned(),
            id: id.to_owned(),
            to: to.into(),
            summary: summary.to_owned(),
            ..UpdateInstructionRequest::default()
        })
        .await
}

async fn claim(
    client: &Client,
    repo: &str,
    request: ClaimInstructionRequest,
) -> Option<v1::Comment> {
    client
        .claim_instruction(ClaimInstructionRequest {
            repo: repo.to_owned(),
            ..request
        })
        .await
        .unwrap()
        .comment
}

async fn get(client: &Client, repo: &str, id: &str) -> v1::Comment {
    client
        .get_comment(GetCommentRequest {
            repo: repo.to_owned(),
            id: id.to_owned(),
        })
        .await
        .unwrap()
        .comment
        .unwrap()
}

fn state(comment: &v1::Comment) -> S {
    comment.instruction.as_ref().unwrap().state()
}

/// Waits until `id` is in `want`.
async fn wait_for(client: &Client, repo: &str, id: &str, want: S) -> v1::Comment {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let comment = get(client, repo, id).await;
        if state(&comment) == want {
            return comment;
        }
        assert!(
            Instant::now() < deadline,
            "{id} stayed {:?}, not {want:?}",
            state(&comment)
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn propose(client: &Client, repo: &str, workspace: &str, revision: &str) {
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            revision: revision.to_owned(),
            to: LifecycleState::Proposed.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
}

/// A workspace whose session declared `client_session`, with `file`
/// written and snapshotted.
async fn session_workspace(client: &Client, repo: &str, client_session: &str) -> Workspace {
    let workspace = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: OWNER.to_owned(),
            provenance: Some(ProvenanceDeclaration {
                client_session: Some(client_session.to_owned()),
                harness: Some("example-harness/1.0".to_owned()),
                model: Some("example-model".to_owned()),
                ..ProvenanceDeclaration::default()
            }),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    std::fs::write(Path::new(&workspace.path).join("work.txt"), "work\n").unwrap();
    let snapshot = client
        .snapshot_workspace("", &workspace.id, "")
        .await
        .unwrap();
    assert!(snapshot.changed);
    workspace
}

// 3.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one note through every check")]
async fn notes_and_anchors() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (base, change) = setup(client, &repo.id).await;

    let written = create(client, &repo.id, note(&change.commit_id, "looks fine"))
        .await
        .unwrap();
    let comment = written.comment.unwrap();
    assert!(comment.id.starts_with("cmt_"));
    assert_eq!(comment.kind(), CommentKind::Note);
    assert_eq!(comment.round, 0);
    assert_eq!(comment.change_id, change.change_id);
    assert_eq!(comment.commit_id, change.commit_id);
    assert!(comment.instruction.is_none());
    assert_eq!(
        comment.author.as_ref().unwrap().actor(),
        v1::TransitionActor::Human
    );
    let operation = written.operation.unwrap();
    assert_eq!(operation.kind(), OperationKind::Comment);
    assert_eq!(operation.comments.len(), 1);
    assert_eq!(operation.comments[0].comment_id, comment.id);
    assert_eq!(operation.comments[0].event, "comment");
    assert!(
        !operation.attributes["reef.comment"].contains("looks fine"),
        "the text stays out of the operation"
    );
    assert_eq!(comment.operation_id, operation.id);

    // Anchors: a new-side range, a removed file on the old side.
    let anchored = create(
        client,
        &repo.id,
        CreateCommentRequest {
            anchor: Some(CommentAnchor {
                path: "a.txt".into(),
                side: LineSide::New.into(),
                start_line: 2,
                end_line: 3,
            }),
            ..note(&change.commit_id, "rename two")
        },
    )
    .await
    .unwrap()
    .comment
    .unwrap();
    assert_eq!(anchored.anchor.as_ref().unwrap().end_line, 3);
    assert!(!anchored.outdated);
    create(
        client,
        &repo.id,
        CreateCommentRequest {
            anchor: Some(CommentAnchor {
                path: "README.md".into(),
                side: LineSide::Old.into(),
                start_line: 1,
                end_line: 0,
            }),
            ..note(&change.commit_id, "why delete this?")
        },
    )
    .await
    .unwrap();
    for (path, side, start, end) in [
        ("a.txt", LineSide::New, 2, 4),
        ("nope.txt", LineSide::New, 0, 0),
        ("README.md", LineSide::New, 0, 0),
        ("", LineSide::New, 1, 1),
        ("a.txt", LineSide::New, 3, 2),
    ] {
        let error = create(
            client,
            &repo.id,
            CreateCommentRequest {
                anchor: Some(CommentAnchor {
                    path: path.into(),
                    side: side.into(),
                    start_line: start,
                    end_line: end,
                }),
                ..note(&change.commit_id, "x")
            },
        )
        .await
        .unwrap_err();
        assert_eq!(
            reason(&error),
            (Code::InvalidArgument, "INVALID_ANCHOR".into()),
            "{path} {start}-{end}"
        );
    }

    // Texts, kinds, replies.
    for text in ["", "  ", "\u{1b}[2J", &"a".repeat(16 * 1024 + 1)] {
        let error = create(client, &repo.id, note(&change.commit_id, text))
            .await
            .unwrap_err();
        assert_eq!(reason(&error).1, "INVALID_TEXT");
    }
    let error = create(
        client,
        &repo.id,
        CreateCommentRequest {
            kind: CommentKind::Unspecified.into(),
            ..note(&change.commit_id, "x")
        },
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&error).1, "KIND_REQUIRED");
    let reply = create(
        client,
        &repo.id,
        CreateCommentRequest {
            reply_to: comment.id.clone(),
            ..note(&change.commit_id, "agreed")
        },
    )
    .await
    .unwrap()
    .comment
    .unwrap();
    let error = create(
        client,
        &repo.id,
        CreateCommentRequest {
            reply_to: comment.id.clone(),
            ..note(&base.commit_id, "elsewhere")
        },
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&error).1, "INVALID_REPLY_TO");
    let read = client
        .get_comment(GetCommentRequest {
            repo: repo.id.clone(),
            id: comment.id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(read.comment.unwrap().text, "looks fine");
    assert_eq!(read.replies.len(), 1);
    assert_eq!(read.replies[0].id, reply.id);

    // The anchored file changes: outdated.
    edit(
        client,
        &repo.id,
        edit_files_request::Target::Revision(change.commit_id.clone()),
        &[("a.txt", Some("one\n2\nthree\n"))],
    )
    .await;
    assert!(get(client, &repo.id, &anchored.id).await.outdated);
    assert!(!get(client, &repo.id, &comment.id).await.outdated);

    // Listing, by change and kind; operations by change and kind.
    let listed = client
        .list_comments(
            ListCommentsRequest {
                repo: repo.id.clone(),
                changes: vec![change.change_id.clone()],
                ..ListCommentsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(listed.len(), 4);
    assert_eq!(listed[0].id, comment.id);
    let none = client
        .list_comments(
            ListCommentsRequest {
                repo: repo.id.clone(),
                kinds: vec![CommentKind::Instruction.into()],
                ..ListCommentsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(none.len(), 0);
    let operations = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            filter: Some(OperationFilter {
                changes: vec![change.change_id.clone()],
                kinds: vec![OperationKind::Comment.into()],
                ..OperationFilter::default()
            }),
            page_size: 100,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations;
    assert_eq!(operations.len(), 4);

    // A comment's operation is not undoable.
    let error = client
        .undo_operation(UndoOperationRequest {
            repo: repo.id.clone(),
            operation_id: operation.id.clone(),
            ..UndoOperationRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, "OPERATION_NOT_UNDOABLE");
}

// 4, 5, 6.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "every move of an instruction")]
async fn instruction_lifecycle() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (_, change) = setup(client, &repo.id).await;
    let change_scope = || ClaimInstructionRequest {
        change: change.commit_id.clone(),
        ..ClaimInstructionRequest::default()
    };

    // Written awaiting confirmation, with what is unknown said.
    let written = create(
        client,
        &repo.id,
        instruction(&change.commit_id, "split a.txt"),
    )
    .await
    .unwrap();
    let comment = written.comment.unwrap();
    assert_eq!(state(&comment), S::AwaitingConfirmation);
    let details = comment.instruction.clone().unwrap();
    assert_eq!(
        details.target.as_ref().unwrap().source(),
        TargetSource::None
    );
    let estimate = details.estimate.unwrap();
    assert!(!estimate.checks_known && !estimate.agent_known);
    assert_eq!(estimate.digest.len(), 64);
    assert!(
        estimate
            .unknown
            .iter()
            .any(|line| line.contains("tokens and price"))
    );
    assert_eq!(written.operation.unwrap().comments[0].event, "instruct");

    // Nothing to take before it is confirmed.
    assert!(claim(client, &repo.id, change_scope()).await.is_none());
    // A claim with no scope at all is refused.
    let error = client
        .claim_instruction(ClaimInstructionRequest {
            repo: repo.id.clone(),
            ..ClaimInstructionRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, "CLAIM_SCOPE_REQUIRED");

    // Refused moves, the expected state, then confirm.
    for to in [S::Running, S::Done] {
        let error = move_to(client, &repo.id, &comment.id, to, "")
            .await
            .unwrap_err();
        assert_eq!(reason(&error).1, "INSTRUCTION_NOT_ALLOWED");
    }
    let error = client
        .update_instruction(UpdateInstructionRequest {
            repo: repo.id.clone(),
            id: comment.id.clone(),
            to: S::Pending.into(),
            expected_state: S::Pending.into(),
            ..UpdateInstructionRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, "EXPECTED_STATE_MISMATCH");
    let confirmed = move_to(client, &repo.id, &comment.id, S::Pending, "")
        .await
        .unwrap();
    assert_eq!(confirmed.operation.unwrap().comments[0].event, "confirm");
    let confirmed = confirmed.comment.unwrap();
    assert_eq!(state(&confirmed), S::Pending);
    assert!(confirmed.instruction.unwrap().confirmed_at.is_some());
    // Confirming again changes nothing.
    assert!(
        move_to(client, &repo.id, &comment.id, S::Pending, "")
            .await
            .unwrap()
            .operation
            .is_none()
    );

    // Claimed (target none: any session on the change), released, claimed
    // and started, done.
    let taken = claim(client, &repo.id, change_scope()).await.unwrap();
    assert_eq!(taken.text, "split a.txt");
    assert_eq!(state(&taken), S::Delivered);
    assert!(!taken.instruction.as_ref().unwrap().taken);
    // A release names the state it releases from; a bare PENDING (a
    // confirmation sent again) never hands a taken instruction out twice.
    let error = move_to(client, &repo.id, &comment.id, S::Pending, "")
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, "INSTRUCTION_NOT_ALLOWED");
    let released = client
        .update_instruction(UpdateInstructionRequest {
            repo: repo.id.clone(),
            id: comment.id.clone(),
            to: S::Pending.into(),
            expected_state: S::Delivered.into(),
            ..UpdateInstructionRequest::default()
        })
        .await
        .unwrap()
        .comment
        .unwrap();
    assert_eq!(state(&released), S::Pending);
    assert!(released.instruction.unwrap().delivered_at.is_none());
    let started = claim(
        client,
        &repo.id,
        ClaimInstructionRequest {
            start: true,
            channel: v1::DeliveryChannel::Cli.into(),
            ..change_scope()
        },
    )
    .await
    .unwrap();
    assert_eq!(state(&started), S::Running);
    assert_eq!(
        started.instruction.as_ref().unwrap().channel(),
        v1::DeliveryChannel::Cli
    );
    let done = move_to(client, &repo.id, &comment.id, S::Done, "split it")
        .await
        .unwrap()
        .comment
        .unwrap();
    assert_eq!(state(&done), S::Done);
    assert_eq!(done.instruction.as_ref().unwrap().summary, "split it");
    let error = move_to(client, &repo.id, &comment.id, S::Cancelled, "")
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, "INSTRUCTION_NOT_ALLOWED");

    // The next instruction's estimate has the agent time of the first.
    let second = create(client, &repo.id, instruction(&change.commit_id, "again"))
        .await
        .unwrap()
        .comment
        .unwrap();
    let estimate = second.instruction.unwrap().estimate.unwrap();
    assert!(estimate.agent_known);
    assert_eq!(estimate.agent_samples, 1);
    assert_eq!(estimate.agent_basis, "change");
    // Cancelled before it runs; a note is not an instruction.
    let cancelled = move_to(client, &repo.id, &second.id, S::Cancelled, "not needed")
        .await
        .unwrap()
        .comment
        .unwrap();
    assert_eq!(state(&cancelled), S::Cancelled);
    let note = create(client, &repo.id, note(&change.commit_id, "n"))
        .await
        .unwrap()
        .comment
        .unwrap();
    let error = move_to(client, &repo.id, &note.id, S::Pending, "")
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, "NOT_AN_INSTRUCTION");

    // Confirming in one call needs ceilings; unknown parts meet them only
    // with allow_unknown.
    let error = create(
        client,
        &repo.id,
        CreateCommentRequest {
            confirm: true,
            ..instruction(&change.commit_id, "x")
        },
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&error).1, "CEILINGS_REQUIRED");
    // Ceilings that bound nothing are no ceilings.
    let error = create(
        client,
        &repo.id,
        CreateCommentRequest {
            confirm: true,
            ceilings: Some(InstructionCeilings {
                allow_unknown: true,
                ..InstructionCeilings::default()
            }),
            ..instruction(&change.commit_id, "x")
        },
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&error).1, "CEILINGS_REQUIRED");
    let refused = create(
        client,
        &repo.id,
        CreateCommentRequest {
            confirm: true,
            ceilings: Some(InstructionCeilings {
                max_check_wall_ms: 60_000,
                ..InstructionCeilings::default()
            }),
            ..instruction(&change.commit_id, "x")
        },
    )
    .await
    .unwrap();
    assert!(refused.confirm_refused.contains("unknown"));
    assert_eq!(
        state(refused.comment.as_ref().unwrap()),
        S::AwaitingConfirmation
    );
    let tight = create(
        client,
        &repo.id,
        CreateCommentRequest {
            confirm: true,
            ceilings: Some(InstructionCeilings {
                max_agent_wall_ms: 1,
                allow_unknown: true,
                ..InstructionCeilings::default()
            }),
            ..instruction(&change.commit_id, "y")
        },
    )
    .await
    .unwrap();
    let agent_wall = tight
        .comment
        .as_ref()
        .unwrap()
        .instruction
        .as_ref()
        .unwrap()
        .estimate
        .as_ref()
        .unwrap()
        .agent_wall_ms;
    if agent_wall > 1 {
        assert!(tight.confirm_refused.contains("ceiling"), "{tight:?}");
    }
    let confirmed = create(
        client,
        &repo.id,
        CreateCommentRequest {
            confirm: true,
            ceilings: Some(InstructionCeilings {
                max_check_wall_ms: 60_000,
                allow_unknown: true,
                ..InstructionCeilings::default()
            }),
            ..instruction(&change.commit_id, "z")
        },
    )
    .await
    .unwrap();
    assert_eq!(confirmed.confirm_refused, "");
    assert_eq!(state(confirmed.comment.as_ref().unwrap()), S::Pending);
}

// 7.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "routing, claims and takes")]
async fn routing_and_claim() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (base, _) = setup(client, &repo.id).await;
    main_at(&repo, &base.commit_id).await;
    let workspace = session_workspace(client, &repo.id, "session-a").await;
    propose(client, &repo.id, &workspace.id, "@").await;
    let proposed = client
        .get_change(v1::GetChangeRequest {
            repo: repo.id.clone(),
            workspace: workspace.id.clone(),
            revision: "@".into(),
            skip_snapshot: true,
            ..v1::GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();

    let written = create(
        client,
        &repo.id,
        CreateCommentRequest {
            confirm: true,
            ceilings: Some(InstructionCeilings {
                max_check_wall_ms: 1,
                allow_unknown: true,
                ..InstructionCeilings::default()
            }),
            ..instruction(&proposed.commit_id, "first")
        },
    )
    .await
    .unwrap()
    .comment
    .unwrap();
    assert_eq!(written.round, 1);
    let target = written
        .instruction
        .as_ref()
        .unwrap()
        .target
        .clone()
        .unwrap();
    assert_eq!(target.source(), TargetSource::Proposal);
    assert_eq!(target.client_session, "session-a");
    assert_eq!(target.harness, "example-harness/1.0");
    assert_eq!(target.workspace_id, workspace.id);
    assert!(target.last_seen_at.is_some());
    let estimate = written
        .instruction
        .as_ref()
        .unwrap()
        .estimate
        .clone()
        .unwrap();
    assert_eq!(estimate.model, "example-model");

    // Another session gets nothing; the target session gets it.
    let other = ClaimInstructionRequest {
        client_session: "session-b".into(),
        ..ClaimInstructionRequest::default()
    };
    assert!(claim(client, &repo.id, other.clone()).await.is_none());
    let own = claim(
        client,
        &repo.id,
        ClaimInstructionRequest {
            client_session: "session-a".into(),
            ..ClaimInstructionRequest::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(own.id, written.id);
    assert_eq!(
        own.instruction.as_ref().unwrap().claimed_client_session,
        "session-a"
    );
    assert!(!own.instruction.as_ref().unwrap().taken);

    // The workspace alone stands for its session.
    let second = create(
        client,
        &repo.id,
        CreateCommentRequest {
            confirm: true,
            ceilings: Some(InstructionCeilings {
                max_check_wall_ms: 1,
                allow_unknown: true,
                ..InstructionCeilings::default()
            }),
            ..instruction(&proposed.commit_id, "second")
        },
    )
    .await
    .unwrap()
    .comment
    .unwrap();
    let third = create(
        client,
        &repo.id,
        CreateCommentRequest {
            confirm: true,
            ceilings: Some(InstructionCeilings {
                max_check_wall_ms: 1,
                allow_unknown: true,
                ..InstructionCeilings::default()
            }),
            ..instruction(&proposed.commit_id, "third")
        },
    )
    .await
    .unwrap()
    .comment
    .unwrap();
    let by_workspace = claim(
        client,
        &repo.id,
        ClaimInstructionRequest {
            workspace: workspace.id.clone(),
            ..ClaimInstructionRequest::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(by_workspace.id, second.id, "oldest first");

    // Another session takes the third, recorded as taken.
    assert!(
        claim(
            client,
            &repo.id,
            ClaimInstructionRequest {
                change: proposed.commit_id.clone(),
                ..other.clone()
            }
        )
        .await
        .is_none()
    );
    let taken = claim(
        client,
        &repo.id,
        ClaimInstructionRequest {
            change: proposed.commit_id.clone(),
            take: true,
            ..other
        },
    )
    .await
    .unwrap();
    assert_eq!(taken.id, third.id);
    assert!(taken.instruction.as_ref().unwrap().taken);

    // A note is never claimed; a comment id that is a note is refused.
    let note = create(client, &repo.id, note(&proposed.commit_id, "fyi"))
        .await
        .unwrap()
        .comment
        .unwrap();
    let error = client
        .claim_instruction(ClaimInstructionRequest {
            repo: repo.id.clone(),
            comment_id: note.id.clone(),
            client_session: "session-a".into(),
            ..ClaimInstructionRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, "NOT_AN_INSTRUCTION");

    // Listing by client session.
    let mine = client
        .list_comments(
            ListCommentsRequest {
                repo: repo.id.clone(),
                client_session: "session-a".into(),
                ..ListCommentsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(mine.len(), 3);
}

// 8.
#[tokio::test(flavor = "multi_thread")]
async fn reproposal_and_final() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (base, change) = setup(client, &repo.id).await;
    let confirm = |text: &str| CreateCommentRequest {
        confirm: true,
        ceilings: Some(InstructionCeilings {
            max_check_wall_ms: 1,
            allow_unknown: true,
            ..InstructionCeilings::default()
        }),
        ..instruction(&change.commit_id, text)
    };
    main_at(&repo, &base.commit_id).await;
    propose(client, &repo.id, "", &change.commit_id).await;
    let delivered = create(client, &repo.id, confirm("fix"))
        .await
        .unwrap()
        .comment
        .unwrap();
    let pending = create(client, &repo.id, confirm("later"))
        .await
        .unwrap()
        .comment
        .unwrap();
    let scope = ClaimInstructionRequest {
        comment_id: delivered.id.clone(),
        change: change.commit_id.clone(),
        ..ClaimInstructionRequest::default()
    };
    claim(client, &repo.id, scope).await.unwrap();

    // Amend, propose again: the delivered one is done, the pending one not.
    let amended = edit(
        client,
        &repo.id,
        edit_files_request::Target::Revision(change.commit_id.clone()),
        &[("a.txt", Some("fixed\n"))],
    )
    .await;
    propose(client, &repo.id, "", &amended.commit_id).await;
    let done = wait_for(client, &repo.id, &delivered.id, S::Done).await;
    assert_eq!(
        done.instruction.as_ref().unwrap().cause(),
        InstructionCause::Reproposed
    );
    assert_eq!(state(&get(client, &repo.id, &pending.id).await), S::Pending);

    // The change lands (seen on trunk()): the pending one is cancelled.
    main_at(&repo, &amended.commit_id).await;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        // A read loads the repository, which the observer follows.
        let read = client
            .get_change(v1::GetChangeRequest {
                repo: repo.id.clone(),
                revision: amended.commit_id.clone(),
                skip_snapshot: true,
                ..v1::GetChangeRequest::default()
            })
            .await
            .unwrap()
            .change
            .unwrap();
        let lifecycle = read.lifecycle.unwrap();
        if lifecycle.recorded && lifecycle.state() == LifecycleState::Landed {
            break;
        }
        assert!(Instant::now() < deadline, "the landing was never observed");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let cancelled = wait_for(client, &repo.id, &pending.id, S::Cancelled).await;
    assert_eq!(
        cancelled.instruction.as_ref().unwrap().cause(),
        InstructionCause::ChangeFinal
    );
    // A landed change takes notes, not instructions.
    create(client, &repo.id, note(&amended.commit_id, "landed fine"))
        .await
        .unwrap();
    let error = create(
        client,
        &repo.id,
        instruction(&amended.commit_id, "too late"),
    )
    .await
    .unwrap_err();
    assert_eq!(reason(&error).1, "CHANGE_FINAL");
}

async fn main_at(repo: &SyntheticRepo, commit: &str) {
    let (root, commit) = (repo.root.clone(), commit.to_owned());
    tokio::task::spawn_blocking(move || set_main(&root, &commit))
        .await
        .unwrap();
}

/// Sets `main` to `commit` from outside the daemon.
fn set_main(root: &Path, commit: &str) {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;
    let settings = reef_daemon::testing::test_settings();
    let workspace = jj_lib::default_backend_factories::default_workspace_loader_factory()
        .create(root)
        .unwrap()
        .load(
            &settings,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .unwrap();
    let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
    let mut tx = repo.start_transaction();
    tx.repo_mut().set_local_bookmark_target(
        RefName::new("main"),
        RefTarget::normal(CommitId::try_from_hex(commit).unwrap()),
    );
    pollster::block_on(tx.commit("bookmark set")).unwrap();
}

// 9.
#[tokio::test(flavor = "multi_thread")]
async fn expiry() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (_, change) = setup(client, &repo.id).await;
    let waiting = create(client, &repo.id, instruction(&change.commit_id, "a"))
        .await
        .unwrap()
        .comment
        .unwrap();
    let running = create(client, &repo.id, instruction(&change.commit_id, "b"))
        .await
        .unwrap()
        .comment
        .unwrap();
    move_to(client, &repo.id, &running.id, S::Pending, "")
        .await
        .unwrap();
    claim(
        client,
        &repo.id,
        ClaimInstructionRequest {
            comment_id: running.id.clone(),
            change: change.commit_id.clone(),
            start: true,
            ..ClaimInstructionRequest::default()
        },
    )
    .await
    .unwrap();
    let state = daemon.handle.state();
    let now = reef_daemon::model::Timestamp::now();
    assert_eq!(reef_daemon::comments::expire(state, now).await.unwrap(), 0);
    let later = reef_daemon::model::Timestamp(now.0 + 8 * 24 * 3_600_000);
    assert_eq!(
        reef_daemon::comments::expire(state, later).await.unwrap(),
        2
    );
    let a = get(client, &repo.id, &waiting.id).await;
    assert_eq!(state_of(&a), (S::Cancelled, InstructionCause::Expired));
    let b = get(client, &repo.id, &running.id).await;
    assert_eq!(state_of(&b), (S::Failed, InstructionCause::Expired));
}

fn state_of(comment: &v1::Comment) -> (S, InstructionCause) {
    let instruction = comment.instruction.as_ref().unwrap();
    (instruction.state(), instruction.cause())
}

// 10.
#[tokio::test(flavor = "multi_thread")]
async fn crash_between_row_and_operation() {
    let (daemon, repo) = fixture().await;
    let (_, change) = setup(&daemon.client, &repo.id).await;
    let repo_id: reef_daemon::model::RepoId = repo.id.parse().unwrap();
    let row = |id: &str| reef_daemon::db::CommentRow {
        repo_id,
        id: id.to_owned(),
        committed: false,
        change_id: change.change_id.clone(),
        round: 0,
        commit_id: change.commit_id.clone(),
        path: None,
        side: None,
        start_line: None,
        end_line: None,
        reply_to: None,
        kind: "note".to_owned(),
        text: "crashed".to_owned(),
        text_sha256: reef_daemon::comments::sha256_hex(b"crashed"),
        agent: None,
        actor: "human".to_owned(),
        identity_id: None,
        identity_name: None,
        session_id: None,
        remote_token: None,
        remote_token_name: None,
        created_at: reef_daemon::model::Timestamp::now(),
        operation_id: None,
        operation_collected: false,
        recorded_at: None,
        instruction: None,
        updated_at: reef_daemon::model::Timestamp::now(),
    };
    let event = |id: &str, comment: &str| reef_daemon::db::CommentEventRow {
        id: id.to_owned(),
        comment_id: comment.to_owned(),
        change_id: change.change_id.clone(),
        event: "comment".to_owned(),
        cause: "request".to_owned(),
        actor: "human".to_owned(),
        ..reef_daemon::db::CommentEventRow::blank(repo_id)
    };
    let db = &daemon.handle.state().db;
    db.insert_pending_comment(
        &row("cmt_00000000000000a1"),
        &event("cev_00000000000000a1", "cmt_00000000000000a1"),
    )
    .unwrap();
    db.insert_pending_comment(
        &row("cmt_00000000000000a2"),
        &event("cev_00000000000000a2", "cmt_00000000000000a2"),
    )
    .unwrap();
    // Only the first's operation was written.
    let root = repo.root.clone();
    let change_id = change.change_id.clone();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let mut jj = reef_lib::jj::Repo::open(&root, &reef_daemon::testing::test_settings())
                .await
                .unwrap();
            let mut meta = reef_lib::jj::OpMeta::new(
                reef_lib::jj::OpKind::Comment,
                "reef: comment comment cmt_00000000000000a1 on change x",
            );
            meta.comment_events = vec![reef_lib::jj::comments::CommentEventRecord {
                id: "cev_00000000000000a1".to_owned(),
                comment: "cmt_00000000000000a1".to_owned(),
                change: change_id,
                round: 0,
                kind: "note".to_owned(),
                event: "comment".to_owned(),
                from: String::new(),
                to: String::new(),
                cause: "request".to_owned(),
            }];
            jj.record_comment_events(&meta).await.unwrap();
        });
    })
    .await
    .unwrap();
    // Reads never see a pending row.
    assert!(
        db.comment(repo_id, "cmt_00000000000000a1")
            .unwrap()
            .is_none()
    );
    let daemon = daemon.restart().await;
    let db = &daemon.handle.state().db;
    assert_eq!(db.pending_comment_events().unwrap().len(), 0);
    let kept = db
        .comment(repo_id, "cmt_00000000000000a1")
        .unwrap()
        .unwrap();
    assert!(kept.operation_id.is_some());
    assert!(
        db.comment(repo_id, "cmt_00000000000000a2")
            .unwrap()
            .is_none()
    );
}

// 12.
#[tokio::test(flavor = "multi_thread")]
async fn remote() {
    let (daemon, repo) = fixture().await;
    let (_, change) = setup(&daemon.client, &repo.id).await;
    daemon
        .client
        .create_identity(CreateIdentityRequest {
            name: "reviewer".into(),
            kind: IdentityKind::Person.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    let plain = remote_client(&daemon, "plain", RemoteScope::Write, "").await;
    let linked = remote_client(&daemon, "linked", RemoteScope::Write, "reviewer").await;
    let reader = remote_client(&daemon, "reader", RemoteScope::Read, "").await;

    let note_by_plain = create(&plain, &repo.id, note(&change.commit_id, "remote note"))
        .await
        .unwrap()
        .comment
        .unwrap();
    assert_eq!(
        note_by_plain.author.as_ref().unwrap().actor(),
        v1::TransitionActor::Agent
    );
    let error = create(&plain, &repo.id, instruction(&change.commit_id, "do it"))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::PermissionDenied, "REMOTE_IDENTITY_REQUIRED".into())
    );
    let by_linked = create(&linked, &repo.id, instruction(&change.commit_id, "do it"))
        .await
        .unwrap()
        .comment
        .unwrap();
    // Only the writing token confirms.
    let error = move_to(&plain, &repo.id, &by_linked.id, S::Pending, "")
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, "REMOTE_COMMENT_NOT_OWNED");
    move_to(&linked, &repo.id, &by_linked.id, S::Pending, "")
        .await
        .unwrap();
    // A claim without take needs the proposing token; nobody proposed.
    let error = plain
        .claim_instruction(ClaimInstructionRequest {
            repo: repo.id.clone(),
            change: change.commit_id.clone(),
            ..ClaimInstructionRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, "REMOTE_COMMENT_NOT_OWNED");
    // Take needs a linked token.
    let error = plain
        .claim_instruction(ClaimInstructionRequest {
            repo: repo.id.clone(),
            change: change.commit_id.clone(),
            take: true,
            ..ClaimInstructionRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, "REMOTE_IDENTITY_REQUIRED");
    let taken = claim(
        &linked,
        &repo.id,
        ClaimInstructionRequest {
            change: change.commit_id.clone(),
            take: true,
            ..ClaimInstructionRequest::default()
        },
    )
    .await
    .unwrap();
    // Only the claiming token moves it after.
    let error = move_to(&plain, &repo.id, &taken.id, S::Running, "")
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, "REMOTE_COMMENT_NOT_OWNED");
    move_to(&linked, &repo.id, &taken.id, S::Done, "")
        .await
        .unwrap();
    // A read token reads.
    let listed = reader
        .list_comments(
            ListCommentsRequest {
                repo: repo.id.clone(),
                ..ListCommentsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(listed.len(), 2);
}

async fn listening(daemon: &TestDaemon) -> SocketAddr {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(addr) = daemon.handle.state().remote.bound_addr() {
            return addr;
        }
        assert!(Instant::now() < deadline, "the remote listener never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn remote_client(
    daemon: &TestDaemon,
    name: &str,
    scope: RemoteScope,
    identity: &str,
) -> Client {
    let addr = listening(daemon).await;
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token_for(name, scope, Some(Duration::from_hours(1)), identity)
        .await
        .unwrap();
    let target = RemoteTarget {
        name: None,
        url: RemoteUrl {
            host: addr.ip().to_string(),
            port: addr.port(),
        },
        fingerprint: Some(fingerprint),
        token: Some(Token::new(&created.secret)),
    };
    Client::connect_remote(&target).await.unwrap()
}

// 13.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_claims() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (_, change) = setup(client, &repo.id).await;
    let comment = create(
        client,
        &repo.id,
        CreateCommentRequest {
            confirm: true,
            ceilings: Some(InstructionCeilings {
                max_check_wall_ms: 1,
                allow_unknown: true,
                ..InstructionCeilings::default()
            }),
            ..instruction(&change.commit_id, "once")
        },
    )
    .await
    .unwrap()
    .comment
    .unwrap();
    let mut tasks = Vec::new();
    for _ in 0..20 {
        let client = client.clone();
        let repo = repo.id.clone();
        let change = change.commit_id.clone();
        tasks.push(tokio::spawn(async move {
            claim(
                &client,
                &repo,
                ClaimInstructionRequest {
                    change,
                    ..ClaimInstructionRequest::default()
                },
            )
            .await
        }));
    }
    let mut won = 0;
    for task in tasks {
        if let Some(claimed) = task.await.unwrap() {
            assert_eq!(claimed.id, comment.id);
            won += 1;
        }
    }
    assert_eq!(won, 1);
}

// 8, through a restart: a re-proposal committed at start, whose notice
// never reached the sweep, still finishes the instruction (reconcile).
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "a crash staged by hand")]
async fn reconcile_after_a_lost_notice() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (base, change) = setup(client, &repo.id).await;
    main_at(&repo, &base.commit_id).await;
    propose(client, &repo.id, "", &change.commit_id).await;
    let written = create(
        client,
        &repo.id,
        CreateCommentRequest {
            confirm: true,
            ceilings: Some(InstructionCeilings {
                max_check_wall_ms: 1,
                allow_unknown: true,
                ..InstructionCeilings::default()
            }),
            ..instruction(&change.commit_id, "fix")
        },
    )
    .await
    .unwrap()
    .comment
    .unwrap();
    claim(
        client,
        &repo.id,
        ClaimInstructionRequest {
            change: change.commit_id.clone(),
            ..ClaimInstructionRequest::default()
        },
    )
    .await
    .unwrap();
    // A re-proposal whose operation was written but whose row is still
    // pending, as a crash between the two leaves it.
    let repo_id: reef_daemon::model::RepoId = repo.id.parse().unwrap();
    let row = reef_daemon::db::TransitionRow {
        repo_id,
        id: "trn_00000000000000c1".to_owned(),
        seq: None,
        committed: false,
        change_id: change.change_id.clone(),
        commit_id: change.commit_id.clone(),
        from_state: "proposed".to_owned(),
        to_state: "proposed".to_owned(),
        cause: "request".to_owned(),
        round: 1,
        reason: None,
        superseded_by: None,
        destination: Some("main".to_owned()),
        undoes_operation_id: None,
        undoes_transition_id: None,
        operation_id: None,
        operation_collected: false,
        agent: None,
        actor: "agent".to_owned(),
        identity_id: None,
        identity_name: None,
        session_id: None,
        remote_token: None,
        remote_token_name: None,
        created_at: reef_daemon::model::Timestamp::now(),
        recorded_at: None,
    };
    daemon
        .handle
        .state()
        .db
        .insert_pending_transition(&row)
        .unwrap();
    let root = repo.root.clone();
    let (change_id, commit_id) = (change.change_id.clone(), change.commit_id.clone());
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let mut jj = reef_lib::jj::Repo::open(&root, &reef_daemon::testing::test_settings())
                .await
                .unwrap();
            let mut meta = reef_lib::jj::OpMeta::new(
                reef_lib::jj::OpKind::Transition,
                "reef: propose change crashed",
            );
            meta.transitions = vec![reef_lib::jj::lifecycle::TransitionRecord {
                id: "trn_00000000000000c1".to_owned(),
                change: change_id,
                commit: commit_id,
                from: reef_lib::jj::lifecycle::LifecycleState::Proposed,
                to: reef_lib::jj::lifecycle::LifecycleState::Proposed,
                cause: reef_lib::jj::lifecycle::TransitionCause::Request,
                round: 1,
                reason: String::new(),
                superseded_by: String::new(),
                destination: "main".to_owned(),
                undoes: String::new(),
            }];
            jj.record_transitions(&meta).await.unwrap();
        });
    })
    .await
    .unwrap();
    let daemon = daemon.restart().await;
    let done = wait_for(&daemon.client, &repo.id, &written.id, S::Done).await;
    assert_eq!(
        done.instruction.as_ref().unwrap().cause(),
        InstructionCause::Reproposed
    );
}
