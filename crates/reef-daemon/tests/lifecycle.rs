//! Change lifecycle states (R3.09, tests 3 to 13): proposals, withdrawals,
//! declines and supersedes through `TransitionChange`; the state on every
//! change; `ListProposals` and `ListTransitions`; observed landings; undo;
//! the abandon refusal; start-up resolution of pending rows; GC; and the
//! remote listener's rules.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, AbandonChangeRequest, CreateWorkspaceRequest, DescribeChangeRequest, EditFilesRequest,
    FileEdit, GetChangeRequest, LifecycleState as State, ListOperationsRequest,
    ListProposalsRequest, ListTransitionsRequest, OperationFilter, OperationKind, RemoteScope,
    TransitionActor, TransitionCause, TransitionChangeRequest, UndoOperationRequest,
    edit_files_request, file_edit,
};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_testing::synth::Preset;
use tonic::Code;

const OWNER: &str = "tester@host";

fn remote_on(config: &mut Config) {
    config.file.remote.enabled = true;
    "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
    config.file.remote.names = vec!["localhost".to_owned()];
}

async fn fixture_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        remote_on(config);
        configure(config);
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
}

fn reason(error: &ClientError) -> (Code, String) {
    match error {
        ClientError::Rpc { status, reason, .. } => (status.code(), reason.clone()),
        other => panic!("{other:?}"),
    }
}

async fn change_at(client: &Client, repo: &str, revision: &str) -> v1::Change {
    client
        .get_change(GetChangeRequest {
            repo: repo.to_owned(),
            revision: revision.to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

/// A new commit on `on` writing each of `files`, outside any workspace.
async fn commit_on(client: &Client, repo: &str, on: &str, files: &[(&str, &str)]) -> v1::Change {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(edit_files_request::Target::NewOn(on.to_owned())),
            edits: files
                .iter()
                .map(|(path, text)| FileEdit {
                    path: (*path).to_owned(),
                    action: Some(file_edit::Action::Content(text.as_bytes().to_vec())),
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

/// Sets a local bookmark from outside the daemon, as `jj bookmark set`
/// would.
fn set_bookmark(root: &Path, name: &str, commit: &str) {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;
    let settings = test_settings();
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
        RefName::new(name),
        RefTarget::normal(CommitId::try_from_hex(commit).unwrap()),
    );
    pollster::block_on(tx.commit("bookmark set")).unwrap();
}

async fn bookmark(repo: &SyntheticRepo, name: &str, commit: &str) {
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || set_bookmark(&root, &name, &commit))
        .await
        .unwrap();
}

/// `main` on a base commit, and two draft changes on it.
struct Setup {
    main: v1::Change,
    change: v1::Change,
    other: v1::Change,
}

async fn setup(daemon: &TestDaemon, repo: &SyntheticRepo) -> Setup {
    let client = &daemon.client;
    let main = commit_on(client, &repo.id, "root()", &[("README.md", "base\n")]).await;
    bookmark(repo, "main", &main.commit_id).await;
    let change = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let other = commit_on(client, &repo.id, &main.commit_id, &[("b.txt", "b\n")]).await;
    Setup {
        main,
        change,
        other,
    }
}

fn to(revision: &str, state: State) -> TransitionChangeRequest {
    TransitionChangeRequest {
        revision: revision.to_owned(),
        to: state.into(),
        ..TransitionChangeRequest::default()
    }
}

async fn transition(
    client: &Client,
    repo: &str,
    request: TransitionChangeRequest,
) -> Result<v1::TransitionChangeResponse, ClientError> {
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.to_owned(),
            ..request
        })
        .await
}

fn state_of(change: &v1::Change) -> State {
    change.lifecycle.as_ref().unwrap().state()
}

async fn transitions(client: &Client, repo: &str, change: &str) -> Vec<v1::ChangeTransition> {
    client
        .list_transitions(
            ListTransitionsRequest {
                repo: repo.to_owned(),
                changes: vec![change.to_owned()],
                ..ListTransitionsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items
}

async fn operations(client: &Client, repo: &str, filter: OperationFilter) -> Vec<v1::Operation> {
    client
        .list_operations_page(ListOperationsRequest {
            repo: repo.to_owned(),
            filter: Some(filter),
            page_size: 1000,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations
}

fn undo(repo: &str, op: &str) -> UndoOperationRequest {
    UndoOperationRequest {
        repo: repo.to_owned(),
        operation_id: op.to_owned(),
        ..UndoOperationRequest::default()
    }
}

// 3.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one proposal through every read")]
async fn propose_and_read() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { main, change, .. } = setup(&daemon, &repo).await;

    // A draft reads draft; main reads landed, derived.
    let draft = change_at(client, &repo.id, &change.commit_id).await;
    assert_eq!(state_of(&draft), State::Draft);
    assert!(!draft.lifecycle.as_ref().unwrap().recorded);
    let trunk = change_at(client, &repo.id, &main.commit_id).await;
    assert_eq!(state_of(&trunk), State::Landed);
    assert!(!trunk.lifecycle.as_ref().unwrap().recorded);

    let proposed = transition(
        client,
        &repo.id,
        TransitionChangeRequest {
            reason: "ready".to_owned(),
            ..to(&change.commit_id, State::Proposed)
        },
    )
    .await
    .unwrap();
    let lifecycle = proposed.lifecycle.clone().unwrap();
    assert_eq!(lifecycle.state(), State::Proposed);
    assert!(lifecycle.recorded);
    assert_eq!(lifecycle.round, 1);
    assert_eq!(lifecycle.destination, "main");
    assert_eq!(lifecycle.commit_id, change.commit_id);
    assert!(!lifecycle.amended);
    let transition_message = proposed.transition.clone().unwrap();
    assert_eq!(transition_message.from(), State::Draft);
    assert_eq!(transition_message.to(), State::Proposed);
    assert_eq!(transition_message.cause(), TransitionCause::Request);
    assert_eq!(transition_message.actor(), TransitionActor::Human);
    assert_eq!(transition_message.reason, "ready");
    assert!(transition_message.id.starts_with("trn_"));
    let operation = proposed.operation.clone().unwrap();
    assert_eq!(operation.kind(), OperationKind::Transition);
    assert_eq!(operation.transitions.len(), 1);
    assert_eq!(operation.transitions[0].id, transition_message.id);
    assert_eq!(operation.transitions[0].change_id, change.change_id);
    assert!(operation.attributes.contains_key("reef.lifecycle"));
    assert_eq!(transition_message.operation_id, operation.id);

    // Every read carries it.
    let read = change_at(client, &repo.id, &change.commit_id).await;
    assert_eq!(state_of(&read), State::Proposed);
    let listed = client
        .list_changes(v1::ListChangesRequest {
            repo: repo.id.clone(),
            revset: change.commit_id.clone(),
            skip_snapshot: true,
            ..v1::ListChangesRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(
        state_of(listed[0].change.as_ref().unwrap()),
        State::Proposed
    );
    let proposals = client
        .list_proposals(
            ListProposalsRequest {
                repo: repo.id.clone(),
                ..ListProposalsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(proposals.len(), 1);
    assert_eq!(proposals[0].change_id, change.change_id);
    assert_eq!(
        proposals[0].change.as_ref().unwrap().commit_id,
        change.commit_id
    );

    // The operation log finds it by change and by kind.
    let by_change = operations(
        client,
        &repo.id,
        OperationFilter {
            changes: vec![change.change_id[..8].to_owned()],
            kinds: vec![OperationKind::Transition.into()],
            ..OperationFilter::default()
        },
    )
    .await;
    assert_eq!(by_change.len(), 1);
    assert_eq!(by_change[0].id, operation.id);
    assert_eq!(by_change[0].transitions.len(), 1);

    // Refusals.
    let refused =
        |result: Result<v1::TransitionChangeResponse, ClientError>| reason(&result.unwrap_err());
    assert_eq!(
        refused(transition(client, &repo.id, to(&main.commit_id, State::Proposed)).await),
        (Code::FailedPrecondition, "IMMUTABLE_REVISION".to_owned())
    );
    assert_eq!(
        refused(transition(client, &repo.id, to(&change.commit_id, State::Queued)).await),
        (Code::InvalidArgument, "TRANSITION_RESERVED".to_owned())
    );
    assert_eq!(
        refused(transition(client, &repo.id, to(&change.commit_id, State::Unspecified)).await),
        (
            Code::InvalidArgument,
            "TRANSITION_TARGET_REQUIRED".to_owned()
        )
    );
    for text in ["x".repeat(4097), "esc \u{1b}[31m".to_owned()] {
        assert_eq!(
            refused(
                transition(
                    client,
                    &repo.id,
                    TransitionChangeRequest {
                        reason: text,
                        ..to(&change.commit_id, State::Withdrawn)
                    }
                )
                .await
            ),
            (Code::InvalidArgument, "INVALID_REASON".to_owned())
        );
    }
    let other = commit_on(client, &repo.id, &main.commit_id, &[("c.txt", "c\n")]).await;
    assert_eq!(
        refused(
            transition(
                client,
                &repo.id,
                TransitionChangeRequest {
                    destination: "nosuch".to_owned(),
                    ..to(&other.commit_id, State::Proposed)
                }
            )
            .await
        ),
        (Code::InvalidArgument, "INVALID_DESTINATION".to_owned())
    );

    // `@` of a fresh workspace is empty; with an edit not yet snapshotted,
    // the proposal snapshots first and records the edit.
    let ws = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            start: Some(v1::create_workspace_request::Start::NewOn("main".into())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    let empty = client
        .transition_change(TransitionChangeRequest {
            workspace: ws.id.clone(),
            ..to("@", State::Proposed)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&empty),
        (Code::FailedPrecondition, "CHANGE_EMPTY".to_owned())
    );
    std::fs::write(Path::new(&ws.path).join("w.txt"), "w\n").unwrap();
    let from_ws = client
        .transition_change(TransitionChangeRequest {
            workspace: ws.id.clone(),
            ..to("@", State::Proposed)
        })
        .await
        .unwrap();
    assert!(from_ws.snapshot.is_some(), "{from_ws:?}");
    let recorded = from_ws.lifecycle.unwrap().commit_id;
    assert_ne!(recorded, ws.commit_id);
    assert_eq!(
        from_ws.operation.unwrap().workspace_id,
        ws.id,
        "the operation names the workspace"
    );
}

// 4.
#[tokio::test(flavor = "multi_thread")]
async fn amend_and_update() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { change, .. } = setup(&daemon, &repo).await;
    transition(client, &repo.id, to(&change.commit_id, State::Proposed))
        .await
        .unwrap();
    let described = client
        .describe_change(DescribeChangeRequest {
            repo: repo.id.clone(),
            revision: change.commit_id.clone(),
            description: "amended\n".to_owned(),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    let lifecycle = described.lifecycle.clone().unwrap();
    assert_eq!(lifecycle.state(), State::Proposed);
    assert!(lifecycle.amended, "{lifecycle:?}");

    let updated = transition(client, &repo.id, to(&described.commit_id, State::Proposed))
        .await
        .unwrap();
    let transition_message = updated.transition.unwrap();
    assert_eq!(transition_message.from(), State::Proposed);
    assert_eq!(transition_message.to(), State::Proposed);
    let lifecycle = updated.lifecycle.unwrap();
    assert!(!lifecycle.amended);
    assert_eq!(lifecycle.round, 1);
    assert_eq!(lifecycle.commit_id, described.commit_id);

    let again = transition(client, &repo.id, to(&described.commit_id, State::Proposed))
        .await
        .unwrap();
    assert!(again.operation.is_none());
    assert!(again.transition.is_none());
}

// 5.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one change through every transition")]
async fn withdraw_decline_supersede() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { change, other, .. } = setup(&daemon, &repo).await;
    let id = change.commit_id.as_str();
    let state = |response: &v1::TransitionChangeResponse| {
        let lifecycle = response.lifecycle.clone().unwrap();
        (lifecycle.state(), lifecycle.round)
    };
    let proposed = transition(client, &repo.id, to(id, State::Proposed))
        .await
        .unwrap();
    assert_eq!(state(&proposed), (State::Proposed, 1));
    let withdrawn = transition(
        client,
        &repo.id,
        TransitionChangeRequest {
            reason: "wrong approach".to_owned(),
            actor: TransitionActor::Agent.into(),
            ..to(id, State::Withdrawn)
        },
    )
    .await
    .unwrap();
    assert_eq!(state(&withdrawn), (State::Withdrawn, 1));
    assert_eq!(
        withdrawn.transition.unwrap().actor(),
        TransitionActor::Agent
    );
    // Withdrawing again records nothing.
    let again = transition(client, &repo.id, to(id, State::Withdrawn))
        .await
        .unwrap();
    assert!(again.operation.is_none());
    let reproposed = transition(client, &repo.id, to(id, State::Proposed))
        .await
        .unwrap();
    assert_eq!(state(&reproposed), (State::Proposed, 2));
    let declined = transition(client, &repo.id, to(id, State::Declined))
        .await
        .unwrap();
    assert_eq!(state(&declined), (State::Declined, 2));
    let third = transition(client, &repo.id, to(id, State::Proposed))
        .await
        .unwrap();
    assert_eq!(state(&third), (State::Proposed, 3));

    let refused =
        |result: Result<v1::TransitionChangeResponse, ClientError>| reason(&result.unwrap_err());
    assert_eq!(
        refused(transition(client, &repo.id, to(id, State::Superseded)).await),
        (Code::InvalidArgument, "SUPERSEDED_BY_REQUIRED".to_owned())
    );
    assert_eq!(
        refused(
            transition(
                client,
                &repo.id,
                TransitionChangeRequest {
                    superseded_by: id.to_owned(),
                    ..to(id, State::Superseded)
                }
            )
            .await
        ),
        (Code::FailedPrecondition, "INVALID_SUPERSEDED_BY".to_owned())
    );
    assert_eq!(
        refused(
            transition(
                client,
                &repo.id,
                TransitionChangeRequest {
                    expected_state: State::Withdrawn.into(),
                    ..to(id, State::Declined)
                }
            )
            .await
        ),
        (
            Code::FailedPrecondition,
            "EXPECTED_STATE_MISMATCH".to_owned()
        )
    );
    let superseded = transition(
        client,
        &repo.id,
        TransitionChangeRequest {
            superseded_by: other.commit_id.clone(),
            expected_state: State::Proposed.into(),
            ..to(id, State::Superseded)
        },
    )
    .await
    .unwrap();
    let lifecycle = superseded.lifecycle.unwrap();
    assert_eq!(lifecycle.state(), State::Superseded);
    assert_eq!(lifecycle.superseded_by, other.change_id);
    assert_eq!(
        refused(transition(client, &repo.id, to(id, State::Proposed)).await),
        (
            Code::FailedPrecondition,
            "TRANSITION_NOT_ALLOWED".to_owned()
        )
    );

    let history = transitions(client, &repo.id, &change.change_id).await;
    let tos: Vec<State> = history.iter().map(v1::ChangeTransition::to).collect();
    assert_eq!(
        tos,
        [
            State::Superseded,
            State::Proposed,
            State::Declined,
            State::Proposed,
            State::Withdrawn,
            State::Proposed
        ]
    );
    assert_eq!(history[4].reason, "wrong approach");
}

// 6.
#[tokio::test(flavor = "multi_thread")]
async fn queued_is_reserved() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { change, .. } = setup(&daemon, &repo).await;
    transition(client, &repo.id, to(&change.commit_id, State::Proposed))
        .await
        .unwrap();
    // As R3.13 will: the change's row says queued.
    let state = daemon.handle.state();
    let repo_id = repo.id.parse().unwrap();
    let mut row = state
        .db
        .lifecycle_row(repo_id, &change.change_id)
        .unwrap()
        .unwrap();
    let queued = reef_daemon::db::TransitionRow {
        repo_id,
        id: "trn_00000000000000aa".to_owned(),
        seq: None,
        committed: false,
        change_id: change.change_id.clone(),
        commit_id: change.commit_id.clone(),
        from_state: "proposed".to_owned(),
        to_state: "queued".to_owned(),
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
        actor: "daemon".to_owned(),
        identity_id: None,
        identity_name: None,
        session_id: None,
        remote_token: None,
        remote_token_name: None,
        created_at: reef_daemon::model::Timestamp::now(),
        recorded_at: None,
    };
    state.db.insert_pending_transition(&queued).unwrap();
    "queued".clone_into(&mut row.state);
    row.transition_id.clone_from(&queued.id);
    state
        .db
        .commit_transition(
            repo_id,
            &queued.id,
            "00",
            reef_daemon::model::Timestamp::now(),
            &reef_daemon::db::LifecycleUpdate::Set(Box::new(row)),
        )
        .unwrap();
    assert_eq!(
        state_of(&change_at(client, &repo.id, &change.commit_id).await),
        State::Queued
    );
    assert_eq!(
        reason(
            &transition(client, &repo.id, to(&change.commit_id, State::Landed))
                .await
                .unwrap_err()
        ),
        (Code::InvalidArgument, "TRANSITION_RESERVED".to_owned())
    );
    let withdrawn = transition(client, &repo.id, to(&change.commit_id, State::Withdrawn))
        .await
        .unwrap();
    assert_eq!(withdrawn.transition.unwrap().from(), State::Queued);
    // Undoing the withdrawal would put the change back in the queue, which
    // only the landing service does.
    assert_eq!(
        reason(
            &client
                .undo_operation(undo(&repo.id, &withdrawn.operation.unwrap().id))
                .await
                .unwrap_err()
        ),
        (Code::InvalidArgument, "OPERATION_NOT_UNDOABLE".to_owned())
    );
}

// 7.
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_transitions() {
    let (daemon, repo) = fixture().await;
    let Setup { change, .. } = setup(&daemon, &repo).await;
    let mut tasks = Vec::new();
    for index in 0..20 {
        let client = daemon.client.clone();
        let (repo_id, commit) = (repo.id.clone(), change.commit_id.clone());
        let state = if index % 2 == 0 {
            State::Proposed
        } else {
            State::Withdrawn
        };
        tasks.push(tokio::spawn(async move {
            transition(&client, &repo_id, to(&commit, state)).await
        }));
    }
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => {}
            Err(error) => assert_eq!(
                reason(&error),
                (
                    Code::FailedPrecondition,
                    "TRANSITION_NOT_ALLOWED".to_owned()
                )
            ),
        }
    }
    let history = transitions(&daemon.client, &repo.id, &change.change_id).await;
    assert_ne!(history.len(), 0);
    // Newest first, each starting where the one before ended.
    for pair in history.windows(2) {
        assert_eq!(pair[0].from(), pair[1].to(), "{history:#?}");
        let (newer, older) = (pair[0].recorded_at.unwrap(), pair[1].recorded_at.unwrap());
        assert!(
            (newer.seconds, newer.nanos) >= (older.seconds, older.nanos),
            "{history:#?}"
        );
    }
    let now = change_at(&daemon.client, &repo.id, &change.commit_id).await;
    assert_eq!(state_of(&now), history[0].to());
}

/// Reads `commit` until its lifecycle is `state`, for at most 10 seconds;
/// each read refreshes the repository, which wakes the observer.
async fn wait_for_state(client: &Client, repo: &str, commit: &str, state: State) -> v1::Change {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let change = change_at(client, repo, commit).await;
        if state_of(&change) == state {
            return change;
        }
        assert!(
            Instant::now() < deadline,
            "never {state:?}: {:?}",
            change.lifecycle
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// 8.
#[tokio::test(flavor = "multi_thread")]
async fn observed_landing() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { change, other, .. } = setup(&daemon, &repo).await;
    transition(client, &repo.id, to(&change.commit_id, State::Proposed))
        .await
        .unwrap();
    bookmark(&repo, "main", &change.commit_id).await;
    let landed = wait_for_state(client, &repo.id, &change.commit_id, State::Landed).await;
    let lifecycle = landed.lifecycle.unwrap();
    assert!(lifecycle.recorded);
    let last = lifecycle.last.unwrap();
    assert_eq!(last.cause(), TransitionCause::Observed);
    assert_eq!(last.actor(), TransitionActor::Daemon);
    assert_eq!(last.from(), State::Proposed);
    assert_eq!(last.destination, "main");
    // Not undoable.
    let refused = client
        .undo_operation(undo(&repo.id, &last.operation_id))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&refused),
        (Code::InvalidArgument, "OPERATION_NOT_UNDOABLE".to_owned())
    );
    // A draft with no record lands derived, with no operation.
    let merge = commit_on(client, &repo.id, &change.commit_id, &[("m.txt", "m\n")]).await;
    let _ = other;
    bookmark(&repo, "main", &merge.commit_id).await;
    let derived = wait_for_state(client, &repo.id, &merge.commit_id, State::Landed).await;
    assert!(!derived.lifecycle.unwrap().recorded);
    assert_eq!(
        transitions(client, &repo.id, &merge.change_id).await.len(),
        0
    );
}

// 9.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "three pending rows through a restart"
)]
async fn crash_between_row_and_operation() {
    let (daemon, repo) = fixture().await;
    let Setup { change, other, .. } = setup(&daemon, &repo).await;
    let repo_id = repo.id.parse().unwrap();
    let pending = |id: &str, change: &v1::Change| reef_daemon::db::TransitionRow {
        repo_id,
        id: id.to_owned(),
        seq: None,
        committed: false,
        change_id: change.change_id.clone(),
        commit_id: change.commit_id.clone(),
        from_state: "draft".to_owned(),
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
        agent: Some("crashed".to_owned()),
        actor: "agent".to_owned(),
        identity_id: None,
        identity_name: None,
        session_id: None,
        remote_token: None,
        remote_token_name: None,
        created_at: reef_daemon::model::Timestamp::now(),
        recorded_at: None,
    };
    let db = &daemon.handle.state().db;
    // One whose operation was written, one whose operation never was.
    let written = pending("trn_00000000000000b1", &change);
    let lost = pending("trn_00000000000000b2", &other);
    // A third, written after the first, whose `from` is no longer the
    // change's state once the first commits: recorded, state unchanged.
    let stale = reef_daemon::db::TransitionRow {
        from_state: "withdrawn".to_owned(),
        to_state: "declined".to_owned(),
        ..pending("trn_00000000000000b3", &change)
    };
    db.insert_pending_transition(&written).unwrap();
    db.insert_pending_transition(&lost).unwrap();
    db.insert_pending_transition(&stale).unwrap();
    let root = repo.root.clone();
    let stale_record = reef_lib::jj::lifecycle::TransitionRecord {
        id: stale.id.clone(),
        change: change.change_id.clone(),
        commit: change.commit_id.clone(),
        from: reef_lib::jj::lifecycle::LifecycleState::Withdrawn,
        to: reef_lib::jj::lifecycle::LifecycleState::Declined,
        cause: reef_lib::jj::lifecycle::TransitionCause::Request,
        round: 1,
        reason: String::new(),
        superseded_by: String::new(),
        destination: "main".to_owned(),
        undoes: String::new(),
    };
    let record = reef_lib::jj::lifecycle::TransitionRecord {
        id: written.id.clone(),
        change: change.change_id.clone(),
        commit: change.commit_id.clone(),
        from: reef_lib::jj::lifecycle::LifecycleState::Draft,
        to: reef_lib::jj::lifecycle::LifecycleState::Proposed,
        cause: reef_lib::jj::lifecycle::TransitionCause::Request,
        round: 1,
        reason: String::new(),
        superseded_by: String::new(),
        destination: "main".to_owned(),
        undoes: String::new(),
    };
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let mut jj = reef_lib::jj::Repo::open(&root, &test_settings())
                .await
                .unwrap();
            let mut meta = reef_lib::jj::OpMeta::new(
                reef_lib::jj::OpKind::Transition,
                "reef: propose change crashed",
            );
            meta.transitions = vec![record];
            jj.record_transitions(&meta).await.unwrap();
            meta.transitions = vec![stale_record];
            jj.record_transitions(&meta).await.unwrap();
        });
    })
    .await
    .unwrap();
    // Reads never see a pending row.
    assert_eq!(
        state_of(&change_at(&daemon.client, &repo.id, &change.commit_id).await),
        State::Draft
    );
    let daemon = daemon.restart().await;
    let db = &daemon.handle.state().db;
    assert_eq!(db.pending_transitions().unwrap(), Vec::new());
    assert_eq!(
        state_of(&change_at(&daemon.client, &repo.id, &change.commit_id).await),
        State::Proposed
    );
    assert_eq!(
        state_of(&change_at(&daemon.client, &repo.id, &other.commit_id).await),
        State::Draft
    );
    assert!(db.transition(repo_id, &lost.id).unwrap().is_none());
    assert!(db.transition(repo_id, &stale.id).unwrap().is_some());
}

// 10.
#[tokio::test(flavor = "multi_thread")]
async fn undo_transitions() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { change, other, .. } = setup(&daemon, &repo).await;
    let id = change.commit_id.as_str();
    transition(client, &repo.id, to(id, State::Proposed))
        .await
        .unwrap();
    let withdraw = transition(client, &repo.id, to(id, State::Withdrawn))
        .await
        .unwrap()
        .operation
        .unwrap();
    let undone = client
        .undo_operation(undo(&repo.id, &withdraw.id))
        .await
        .unwrap();
    assert_eq!(undone.undone_operation_id, withdraw.id);
    let undo_op = undone.operation.unwrap();
    assert_eq!(undo_op.kind(), OperationKind::Undo);
    assert_eq!(undo_op.transitions[0].cause(), TransitionCause::Undo);
    let now = change_at(client, &repo.id, id).await;
    let lifecycle = now.lifecycle.unwrap();
    assert_eq!((lifecycle.state(), lifecycle.round), (State::Proposed, 1));
    // Undoing the undo withdraws again.
    client
        .undo_operation(undo(&repo.id, &undo_op.id))
        .await
        .unwrap();
    assert_eq!(
        state_of(&change_at(client, &repo.id, id).await),
        State::Withdrawn
    );
    // The withdrawal is no longer the latest: superseded.
    transition(client, &repo.id, to(id, State::Proposed))
        .await
        .unwrap();
    assert_eq!(
        reason(
            &client
                .undo_operation(undo(&repo.id, &withdraw.id))
                .await
                .unwrap_err()
        ),
        (Code::FailedPrecondition, "OPERATION_SUPERSEDED".to_owned())
    );
    // Undoing a first proposal returns the change to draft.
    let first = transition(client, &repo.id, to(&other.commit_id, State::Proposed))
        .await
        .unwrap()
        .operation
        .unwrap();
    client
        .undo_operation(undo(&repo.id, &first.id))
        .await
        .unwrap();
    let draft = change_at(client, &repo.id, &other.commit_id).await;
    assert_eq!(state_of(&draft), State::Draft);
    assert!(!draft.lifecycle.unwrap().recorded);
    // Rounds never repeat: proposing it again is round 2.
    let again = transition(client, &repo.id, to(&other.commit_id, State::Proposed))
        .await
        .unwrap();
    assert_eq!(again.lifecycle.unwrap().round, 2);
    // `--kind transition` keeps the undos, which carry transitions too.
    let kept = operations(
        client,
        &repo.id,
        OperationFilter {
            changes: vec![other.change_id.clone()],
            kinds: vec![OperationKind::Transition.into()],
            ..OperationFilter::default()
        },
    )
    .await;
    let kinds: Vec<OperationKind> = kept.iter().map(v1::Operation::kind).collect();
    assert_eq!(
        kinds,
        [
            OperationKind::Transition,
            OperationKind::Undo,
            OperationKind::Transition
        ]
    );
}

// The review's finding 4: a proposal whose change lost its visible commit
// (here by undoing the edit that created it) still leaves review, by its
// change id.
#[tokio::test(flavor = "multi_thread")]
async fn a_stranded_proposal_leaves_review() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { main, .. } = setup(&daemon, &repo).await;
    let created = client
        .edit_files(EditFilesRequest {
            repo: repo.id.clone(),
            target: Some(edit_files_request::Target::NewOn(main.commit_id.clone())),
            edits: vec![FileEdit {
                path: "s.txt".to_owned(),
                action: Some(file_edit::Action::Content(b"s\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap();
    let change = created.change.unwrap();
    transition(client, &repo.id, to(&change.commit_id, State::Proposed))
        .await
        .unwrap();
    client
        .undo_operation(undo(&repo.id, &created.operation.unwrap().id))
        .await
        .unwrap();
    let listed = client
        .list_proposals(
            ListProposalsRequest {
                repo: repo.id.clone(),
                ..ListProposalsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(listed.len(), 1);
    assert!(listed[0].change.is_none());
    assert!(!listed[0].lifecycle.as_ref().unwrap().visible);
    let withdrawn = transition(client, &repo.id, to(&change.change_id, State::Withdrawn))
        .await
        .unwrap();
    assert!(withdrawn.change.is_none());
    let lifecycle = withdrawn.lifecycle.unwrap();
    assert_eq!(lifecycle.state(), State::Withdrawn);
    assert!(!lifecycle.visible);
    // It cannot be proposed again with no commit.
    assert_eq!(
        reason(
            &transition(client, &repo.id, to(&change.change_id, State::Proposed))
                .await
                .unwrap_err()
        )
        .0,
        Code::NotFound
    );
}

// Paging and filters of the two listings.
#[tokio::test(flavor = "multi_thread")]
async fn listings_page_and_filter() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { change, other, .. } = setup(&daemon, &repo).await;
    for id in [&change.commit_id, &other.commit_id] {
        transition(client, &repo.id, to(id, State::Proposed))
            .await
            .unwrap();
    }
    let first = client
        .list_proposals_page(ListProposalsRequest {
            repo: repo.id.clone(),
            page_size: 1,
            ..ListProposalsRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 1);
    assert_ne!(first.next_page_token, "");
    let second = client
        .list_proposals_page(ListProposalsRequest {
            repo: repo.id.clone(),
            page_size: 1,
            page_token: first.next_page_token,
            ..ListProposalsRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(second.entries.len(), 1);
    assert_ne!(second.entries[0].change_id, first.entries[0].change_id);
    let marker = transitions(client, &repo.id, &other.change_id).await[0]
        .recorded_at
        .unwrap();
    transition(client, &repo.id, to(&change.commit_id, State::Withdrawn))
        .await
        .unwrap();
    let after = client
        .list_transitions(
            ListTransitionsRequest {
                repo: repo.id.clone(),
                recorded_after: Some(marker),
                ..ListTransitionsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(after.len(), 1, "{after:#?}");
    assert_eq!(after[0].to(), State::Withdrawn);
    let proposals = client
        .list_transitions(
            ListTransitionsRequest {
                repo: repo.id.clone(),
                to: vec![State::Proposed.into()],
                ..ListTransitionsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(proposals.len(), 2);
    let by_agent = client
        .list_proposals(
            ListProposalsRequest {
                repo: repo.id.clone(),
                agents: vec!["nobody".to_owned()],
                ..ListProposalsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(by_agent.len(), 0);
}

// 11.
#[tokio::test(flavor = "multi_thread")]
async fn abandon_in_review() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { change, .. } = setup(&daemon, &repo).await;
    transition(client, &repo.id, to(&change.commit_id, State::Proposed))
        .await
        .unwrap();
    let abandon = AbandonChangeRequest {
        repo: repo.id.clone(),
        revision: change.commit_id.clone(),
        ..AbandonChangeRequest::default()
    };
    let refused = client.abandon_change(abandon.clone()).await.unwrap_err();
    assert_eq!(
        reason(&refused),
        (Code::FailedPrecondition, "CHANGE_IN_REVIEW".to_owned())
    );
    match &refused {
        ClientError::Rpc { status, .. } => {
            let next = reef_api::error::detail(status).unwrap().next;
            assert_eq!(next[..3], ["reef", "proposal", "withdraw"], "{next:?}");
        }
        other => panic!("{other:?}"),
    }
    transition(client, &repo.id, to(&change.commit_id, State::Withdrawn))
        .await
        .unwrap();
    client.abandon_change(abandon).await.unwrap();
}

// 12.
#[tokio::test(flavor = "multi_thread")]
async fn gc_and_replay() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.undo_window = Duration::ZERO;
    })
    .await;
    let client = &daemon.client;
    let Setup { change, .. } = setup(&daemon, &repo).await;
    let before = client.get_operation(&repo.id, "").await.ok();
    let proposed = transition(client, &repo.id, to(&change.commit_id, State::Proposed))
        .await
        .unwrap()
        .operation
        .unwrap();
    // Replay: before the proposal, a draft; at it, proposed.
    let at = |operation: String| GetChangeRequest {
        repo: repo.id.clone(),
        revision: change.commit_id.clone(),
        at_operation: operation,
        ..GetChangeRequest::default()
    };
    let parent = proposed.parent_ids[0].clone();
    let then = client.get_change(at(parent)).await.unwrap().change.unwrap();
    assert_eq!(state_of(&then), State::Draft);
    let now = client
        .get_change(at(proposed.id.clone()))
        .await
        .unwrap()
        .change
        .unwrap();
    assert_eq!(state_of(&now), State::Proposed);
    let _ = before;

    // GC drops the proposal's operation once it is older than the window,
    // and gives the head a new id.
    let withdrawn = transition(client, &repo.id, to(&change.commit_id, State::Withdrawn))
        .await
        .unwrap()
        .operation
        .unwrap();
    tokio::time::sleep(Duration::from_millis(1100)).await;
    client.collect_garbage(&repo.id, false).await.unwrap();
    let history = transitions(client, &repo.id, &change.change_id).await;
    assert_eq!(history.len(), 2);
    assert!(history[1].operation_collected, "{history:#?}");
    assert_eq!(history[1].operation_id, proposed.id);
    // The head (the withdrawal) is kept under a new id, which the row now
    // names.
    assert!(!history[0].operation_collected);
    assert_ne!(history[0].operation_id, withdrawn.id);
    let kept = client
        .get_operation(&repo.id, &history[0].operation_id)
        .await
        .unwrap()
        .operation
        .unwrap();
    assert_eq!(kept.transitions[0].id, history[0].id);
    assert_eq!(
        state_of(&change_at(client, &repo.id, &change.commit_id).await),
        State::Withdrawn
    );
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

async fn remote_client(daemon: &TestDaemon, name: &str, scope: RemoteScope) -> Client {
    let addr = listening(daemon).await;
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token_for(name, scope, Some(Duration::from_hours(1)), "")
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
#[expect(clippy::too_many_lines, reason = "every remote rule in one fixture")]
async fn remote() {
    let (daemon, repo) = fixture().await;
    let Setup { change, .. } = setup(&daemon, &repo).await;
    let alpha = remote_client(&daemon, "alpha", RemoteScope::Write).await;
    let beta = remote_client(&daemon, "beta", RemoteScope::Write).await;
    let reader = remote_client(&daemon, "reader", RemoteScope::Read).await;
    let id = change.commit_id.as_str();

    let proposed = transition(&alpha, &repo.id, to(id, State::Proposed))
        .await
        .unwrap();
    let message = proposed.transition.unwrap();
    assert_eq!(message.actor(), TransitionActor::Agent);
    assert_eq!(message.remote.unwrap().token_name, "alpha");
    assert_eq!(
        reason(
            &transition(&beta, &repo.id, to(id, State::Withdrawn))
                .await
                .unwrap_err()
        ),
        (
            Code::PermissionDenied,
            "REMOTE_PROPOSAL_NOT_OWNED".to_owned()
        )
    );
    assert_eq!(
        reason(
            &transition(&alpha, &repo.id, to(id, State::Declined))
                .await
                .unwrap_err()
        ),
        (Code::PermissionDenied, "REMOTE_HUMAN_REQUIRED".to_owned())
    );
    // A local update of the commit, and its undo, leave the proposal
    // alpha's.
    let amended = daemon
        .client
        .describe_change(DescribeChangeRequest {
            repo: repo.id.clone(),
            revision: change.commit_id.clone(),
            description: "amended\n".to_owned(),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    let update = transition(
        &daemon.client,
        &repo.id,
        to(&amended.commit_id, State::Proposed),
    )
    .await
    .unwrap()
    .operation
    .unwrap();
    daemon
        .client
        .undo_operation(undo(&repo.id, &update.id))
        .await
        .unwrap();
    let id = amended.commit_id.as_str();
    let withdrawn = transition(&alpha, &repo.id, to(id, State::Withdrawn))
        .await
        .unwrap()
        .operation
        .unwrap();
    assert_eq!(
        reason(
            &beta
                .undo_operation(undo(&repo.id, &withdrawn.id))
                .await
                .unwrap_err()
        ),
        (
            Code::PermissionDenied,
            "REMOTE_PROPOSAL_NOT_OWNED".to_owned()
        )
    );
    alpha
        .undo_operation(undo(&repo.id, &withdrawn.id))
        .await
        .unwrap();
    let listed = reader
        .list_proposals(
            ListProposalsRequest {
                repo: repo.id.clone(),
                ..ListProposalsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].proposed_by, "alpha");
    assert_eq!(
        reason(
            &transition(&reader, &repo.id, to(id, State::Withdrawn))
                .await
                .unwrap_err()
        )
        .0,
        Code::PermissionDenied
    );
}
