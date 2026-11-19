//! The dependent change lifecycle (R3.11, ADR 0084): amending the bottom
//! of a stack held in three workspaces (Phase 3's exit criterion); a
//! declined dependency returning its dependents; a landing and a supersede
//! carrying the proposals above them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::future::Future;
use std::path::Path;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, ConflictKind, CreateWorkspaceRequest, DependencyEvent, DescribeChangeRequest,
    EditFilesRequest, FileEdit, LandChangeRequest, LandingOutcome, LifecycleState as State,
    TransitionCause, TransitionChangeRequest, create_workspace_request, edit_files_request,
    file_edit,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_testing::synth::Preset;

const POLICY: &str = r#"
version = 1

[owners]
identities = ["alice"]

[landing]
checks = "proposal"
"#;

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

/// Waits up to twenty seconds for `ready`.
async fn eventually<F, Fut>(what: &str, mut ready: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    while !ready().await {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn reason(
    error: &ClientError,
) -> (
    String,
    std::collections::BTreeMap<String, String>,
    Vec<String>,
) {
    let next = error.advice().map(|advice| advice.next).unwrap_or_default();
    match error {
        ClientError::Rpc {
            reason, metadata, ..
        } => (reason.clone(), metadata.clone(), next),
        other => panic!("{other:?}"),
    }
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

/// Sets a local bookmark from outside the daemon, as `jj bookmark set`.
async fn bookmark(repo: &SyntheticRepo, name: &str, commit: &str) {
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || {
        use jj_lib::backend::CommitId;
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
        };
        use jj_lib::op_store::RefTarget;
        use jj_lib::ref_name::RefName;
        let settings = test_settings();
        let workspace = jj_lib::default_backend_factories::default_workspace_loader_factory()
            .create(&root)
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
            RefName::new(&name),
            RefTarget::normal(CommitId::try_from_hex(&commit).unwrap()),
        );
        pollster::block_on(tx.commit("bookmark set")).unwrap();
    })
    .await
    .unwrap();
}

/// Deletes a local bookmark from outside the daemon, as `jj bookmark
/// delete`.
async fn delete_bookmark(repo: &SyntheticRepo, name: &str) {
    let (root, name) = (repo.root.clone(), name.to_owned());
    tokio::task::spawn_blocking(move || {
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
        };
        use jj_lib::op_store::RefTarget;
        use jj_lib::ref_name::RefName;
        let settings = test_settings();
        let workspace = jj_lib::default_backend_factories::default_workspace_loader_factory()
            .create(&root)
            .unwrap()
            .load(
                &settings,
                &default_backend_factories(),
                &default_working_copy_factories(),
            )
            .unwrap();
        let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
        let mut tx = repo.start_transaction();
        tx.repo_mut()
            .set_local_bookmark_target(RefName::new(&name), RefTarget::absent());
        pollster::block_on(tx.commit("bookmark delete")).unwrap();
    })
    .await
    .unwrap();
}

/// Makes `change`'s commit divergent from outside the daemon: another
/// visible commit with the same change id, on the same parents.
async fn duplicate_change(repo: &SyntheticRepo, commit: &str) {
    let (root, commit) = (repo.root.clone(), commit.to_owned());
    tokio::task::spawn_blocking(move || {
        use jj_lib::backend::CommitId;
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
        };
        use jj_lib::repo::Repo as _;
        let settings = test_settings();
        let workspace = jj_lib::default_backend_factories::default_workspace_loader_factory()
            .create(&root)
            .unwrap()
            .load(
                &settings,
                &default_backend_factories(),
                &default_working_copy_factories(),
            )
            .unwrap();
        let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
        let old = pollster::block_on(
            repo.store()
                .get_commit_async(&CommitId::try_from_hex(&commit).unwrap()),
        )
        .unwrap();
        let mut tx = repo.start_transaction();
        let _dup = pollster::block_on(
            tx.repo_mut()
                .new_commit(old.parent_ids().to_vec(), old.tree())
                .set_change_id(old.change_id().clone())
                .set_description("a duplicate")
                .write(),
        )
        .unwrap();
        pollster::block_on(tx.commit("duplicate")).unwrap();
    })
    .await
    .unwrap();
}

/// `main` with the policy on a base commit, holding `f.txt`.
async fn setup(client: &Client, repo: &SyntheticRepo) -> v1::Change {
    client
        .create_identity(v1::CreateIdentityRequest {
            name: "alice".to_owned(),
            kind: v1::IdentityKind::Person.into(),
            ..v1::CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[
            ("README.md", "base\n"),
            ("f.txt", "1\n2\n3\n"),
            ("reef-policy.toml", POLICY),
        ],
    )
    .await;
    bookmark(repo, "main", &main.commit_id).await;
    main
}

fn transition(repo: &str, revision: &str, to: State, stack: bool) -> TransitionChangeRequest {
    TransitionChangeRequest {
        repo: repo.to_owned(),
        revision: revision.to_owned(),
        to: to.into(),
        stack,
        ..TransitionChangeRequest::default()
    }
}

async fn change(client: &Client, repo: &str, revision: &str) -> v1::Change {
    client
        .get_change(v1::GetChangeRequest {
            repo: repo.to_owned(),
            revision: revision.to_owned(),
            skip_snapshot: true,
            ..v1::GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

async fn lifecycle(client: &Client, repo: &str, change_id: &str) -> v1::ChangeLifecycle {
    change(client, repo, change_id).await.lifecycle.unwrap()
}

/// A workspace on `on` that writes `files`, snapshots and describes.
async fn worker(
    client: &Client,
    repo: &SyntheticRepo,
    owner: &str,
    on: &str,
    files: &[(&str, &str)],
) -> v1::Workspace {
    let ws = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: owner.to_owned(),
            start: Some(create_workspace_request::Start::NewOn(on.to_owned())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    write_files(&ws, files);
    client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    client
        .describe_change(DescribeChangeRequest {
            repo: repo.id.clone(),
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            description: format!("{owner}'s change\n"),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap();
    client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap()
}

fn write_files(ws: &v1::Workspace, files: &[(&str, &str)]) {
    for (path, text) in files {
        std::fs::write(Path::new(&ws.path).join(path), text).unwrap();
    }
}

fn read(ws: &v1::Workspace, path: &str) -> String {
    std::fs::read_to_string(Path::new(&ws.path).join(path)).unwrap_or_default()
}

async fn conflicts(client: &Client, repo: &str) -> Vec<v1::Conflict> {
    client
        .list_conflicts(
            v1::ListConflictsRequest {
                repo: repo.to_owned(),
                skip_snapshot: true,
                ..v1::ListConflictsRequest::default()
            },
            100,
        )
        .await
        .unwrap()
        .items
}

async fn assignments(client: &Client, repo: &str) -> Vec<v1::ConflictAssignment> {
    client
        .list_conflict_assignments(
            v1::ListConflictAssignmentsRequest {
                repo: repo.to_owned(),
                ..v1::ListConflictAssignmentsRequest::default()
            },
            100,
        )
        .await
        .unwrap()
        .items
}

async fn instructions(client: &Client, repo: &str, change_id: &str) -> Vec<v1::Comment> {
    client
        .list_comments(
            v1::ListCommentsRequest {
                repo: repo.to_owned(),
                changes: vec![change_id.to_owned()],
                kinds: vec![v1::CommentKind::Instruction.into()],
                ..v1::ListCommentsRequest::default()
            },
            100,
        )
        .await
        .unwrap()
        .items
}

// R3.11 test 2, Phase 3's exit criterion: amending the bottom of a stack
// held in three workspaces rebases the changes above it in the same
// operation, keeps their ids, keeps their workspaces current, carries the
// one that still applies, and leaves a conflict object, assigned to its
// owner, on the one that does not.
#[tokio::test(flavor = "multi_thread")]
async fn amend_bottom_of_stack() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo).await;
    let a = worker(
        client,
        &repo,
        "a@test",
        &main.commit_id,
        &[("a.txt", "a\n")],
    )
    .await;
    let b = worker(client, &repo, "b@test", &a.commit_id, &[("b.txt", "b\n")]).await;
    let c = worker(
        client,
        &repo,
        "c@test",
        &b.commit_id,
        &[("f.txt", "1\nC\n3\n")],
    )
    .await;
    client
        .transition_change(transition(&repo.id, &c.change_id, State::Proposed, true))
        .await
        .unwrap();

    // A edits the line C edited: C no longer applies. B touches neither.
    write_files(&a, &[("a.txt", "a2\n"), ("f.txt", "1\nA\n3\n")]);
    client
        .snapshot_workspace(&repo.id, &a.id, "")
        .await
        .unwrap();

    eventually("B is carried", || async {
        let b_now = lifecycle(client, &repo.id, &b.change_id).await;
        !b_now.amended
            && b_now
                .last
                .as_ref()
                .is_some_and(|last| last.cause() == TransitionCause::Dependency)
    })
    .await;
    let b_now = change(client, &repo.id, &b.change_id).await;
    assert!(!b_now.divergent && !b_now.conflict, "{b_now:?}");
    let last = b_now.lifecycle.unwrap().last.unwrap();
    assert_eq!(last.actor(), v1::TransitionActor::Daemon);
    let dependency = last.dependency.unwrap();
    assert_eq!(dependency.change_id, a.change_id);
    assert_eq!(dependency.event(), DependencyEvent::Amended);

    let c_now = change(client, &repo.id, &c.change_id).await;
    assert_eq!(c_now.change_id, c.change_id);
    assert!(c_now.conflict && !c_now.divergent, "{c_now:?}");
    assert!(c_now.lifecycle.unwrap().amended, "C is not carried");

    // The workspaces above follow: B has A's new file, C has the markers.
    eventually("B's and C's directories follow", || async {
        read(&b, "a.txt") == "a2\n" && read(&c, "f.txt").contains("<<<<<<<")
    })
    .await;

    let found = conflicts(client, &repo.id).await;
    let tree = found
        .iter()
        .find(|conflict| conflict.kind() == ConflictKind::Tree)
        .expect("C's conflict");
    assert_eq!(tree.change.as_ref().unwrap().change_id, c.change_id);
    let assigned = assignments(client, &repo.id).await;
    let c_assignment = assigned
        .iter()
        .find(|row| row.change_id == c.change_id)
        .expect("C's assignment");
    assert_eq!(
        c_assignment.owner.as_ref().unwrap().workspace_id,
        c.id,
        "{c_assignment:?}"
    );

    let c_ws = client
        .get_workspace(v1::GetWorkspaceRequest {
            repo: repo.id.clone(),
            workspace: c.id.clone(),
            ..v1::GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(c_ws.step(), v1::ChangeStep::Resolve);

    eventually("C's session is told", || async {
        instructions(client, &repo.id, &c.change_id)
            .await
            .iter()
            .any(|comment| comment.text.contains("no longer applies"))
    })
    .await;
    assert!(
        instructions(client, &repo.id, &b.change_id)
            .await
            .is_empty(),
        "a clean carry tells nobody"
    );
    daemon.stop().await;
}

// R3.11 test 5: a declined dependency holds the changes above it, each
// with a missing-dependency conflict and an assignment; landing or
// proposing the stack says how to move off it, and moving off closes it.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "each way the return shows, then moving off, in order"
)]
async fn declined_dependency_returns_the_dependents() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo).await;
    let a = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let b = commit_on(client, &repo.id, &a.commit_id, &[("b.txt", "b\n")]).await;
    let c = commit_on(client, &repo.id, &b.commit_id, &[("c.txt", "c\n")]).await;
    client
        .transition_change(transition(&repo.id, &c.change_id, State::Proposed, true))
        .await
        .unwrap();
    client
        .transition_change(transition(&repo.id, &a.change_id, State::Declined, false))
        .await
        .unwrap();

    let found = conflicts(client, &repo.id).await;
    let missing: Vec<&v1::Conflict> = found
        .iter()
        .filter(|conflict| conflict.kind() == ConflictKind::MissingDependency)
        .collect();
    assert_eq!(missing.len(), 2, "{found:?}");
    for conflict in &missing {
        let dependency = conflict.dependency.as_ref().unwrap();
        assert_eq!(dependency.change_id, a.change_id);
        assert_eq!(dependency.state(), State::Declined);
        assert_eq!(dependency.move_change_id, b.change_id);
        assert_eq!(dependency.move_onto, "main");
        assert!(conflict.id.starts_with("cfl_"));
    }
    let single = client
        .get_conflict(v1::GetConflictRequest {
            repo: repo.id.clone(),
            revision: c.change_id.clone(),
            kind: ConflictKind::MissingDependency.into(),
            skip_snapshot: true,
            ..v1::GetConflictRequest::default()
        })
        .await
        .unwrap()
        .conflict
        .unwrap();
    assert_eq!(single.change.unwrap().change_id, c.change_id);
    let dependency_rows: Vec<v1::ConflictAssignment> = assignments(client, &repo.id)
        .await
        .into_iter()
        .filter(|row| row.kind() == ConflictKind::MissingDependency)
        .collect();
    assert_eq!(dependency_rows.len(), 2, "{dependency_rows:?}");

    let landing = client
        .land_change(LandChangeRequest {
            repo: repo.id.clone(),
            revision: c.change_id.clone(),
            ..LandChangeRequest::default()
        })
        .await
        .unwrap_err();
    let (why, metadata, next) = reason(&landing);
    assert_eq!(why, "DEPENDENCY_LEFT_REVIEW");
    assert_eq!(metadata.get("change_id"), Some(&a.change_id));
    assert_eq!(next[..3], ["reef", "rebase", b.change_id.as_str()]);

    let proposing = client
        .transition_change(transition(&repo.id, &c.change_id, State::Proposed, true))
        .await
        .unwrap_err();
    assert_eq!(reason(&proposing).0, "MEMBER_LEFT_REVIEW");
    assert_eq!(
        lifecycle(client, &repo.id, &a.change_id).await.state(),
        State::Declined,
        "the decline stands"
    );

    let entries = client
        .list_proposals(
            v1::ListProposalsRequest {
                repo: repo.id.clone(),
                changes: vec![c.change_id.clone()],
                ..v1::ListProposalsRequest::default()
            },
            10,
        )
        .await
        .unwrap()
        .items;
    let dependencies = &entries[0].dependencies;
    assert_eq!(
        dependencies
            .iter()
            .map(|dependency| (
                dependency.change_id.clone(),
                dependency.lifecycle.as_ref().unwrap().state()
            ))
            .collect::<Vec<_>>(),
        vec![
            (a.change_id.clone(), State::Declined),
            (b.change_id.clone(), State::Proposed)
        ]
    );

    // The `next`: B and C move off A, onto main.
    client
        .rebase_change(v1::RebaseChangeRequest {
            repo: repo.id.clone(),
            revision: b.change_id.clone(),
            onto: "main".to_owned(),
            mode: v1::RebaseMode::Source.into(),
            skip_fetch: true,
            ..v1::RebaseChangeRequest::default()
        })
        .await
        .unwrap();
    assert!(
        conflicts(client, &repo.id)
            .await
            .iter()
            .all(|conflict| conflict.kind() != ConflictKind::MissingDependency)
    );
    let closed: Vec<v1::ConflictAssignment> = assignments(client, &repo.id)
        .await
        .into_iter()
        .filter(|row| row.kind() == ConflictKind::MissingDependency)
        .collect();
    assert!(
        closed
            .iter()
            .all(|row| row.state() == v1::ConflictAssignmentState::Closed
                && row.reason() == v1::ConflictAssignmentReason::Resolved),
        "{closed:?}"
    );
    daemon.stop().await;
}

// R3.11 test 4: a dependent proposed on its own, under a dependency that
// lands after the destination moved, is carried across the landing's
// rewrite and lands with no new proposal (R3.10's order by the edge).
#[tokio::test(flavor = "multi_thread")]
async fn landing_carries_the_dependent() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo).await;
    let a = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let b = commit_on(client, &repo.id, &a.commit_id, &[("b.txt", "b\n")]).await;
    client
        .transition_change(transition(&repo.id, &a.change_id, State::Proposed, false))
        .await
        .unwrap();
    client
        .transition_change(transition(&repo.id, &b.change_id, State::Proposed, false))
        .await
        .unwrap();
    // The destination moves on, so A lands rebased.
    let moved = commit_on(client, &repo.id, &main.commit_id, &[("m.txt", "m\n")]).await;
    bookmark(&repo, "main", &moved.commit_id).await;

    let landed = client
        .land_change(LandChangeRequest {
            repo: repo.id.clone(),
            revision: a.change_id.clone(),
            wait_ms: 60_000,
            ..LandChangeRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(landed.landing.unwrap().outcome(), LandingOutcome::Landed);

    eventually("B is carried", || async {
        !lifecycle(client, &repo.id, &b.change_id).await.amended
    })
    .await;
    let b_now = lifecycle(client, &repo.id, &b.change_id).await;
    let dependency = b_now.last.unwrap().dependency.unwrap();
    assert_eq!(dependency.change_id, a.change_id);
    assert_eq!(dependency.event(), DependencyEvent::Landed);
    let landed = client
        .land_change(LandChangeRequest {
            repo: repo.id.clone(),
            revision: b.change_id.clone(),
            wait_ms: 60_000,
            ..LandChangeRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(landed.landing.unwrap().outcome(), LandingOutcome::Landed);
    daemon.stop().await;
}

// R3.11 test 9: superseding a change rebases its dependents onto the
// superseding change in the same operation, and carries the one whose own
// diff still applies.
#[tokio::test(flavor = "multi_thread")]
async fn supersede_moves_the_dependents() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo).await;
    let a = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let b = commit_on(client, &repo.id, &a.commit_id, &[("b.txt", "b\n")]).await;
    let a2 = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a2\n")]).await;
    client
        .transition_change(transition(&repo.id, &b.change_id, State::Proposed, true))
        .await
        .unwrap();

    let superseded = client
        .transition_change(TransitionChangeRequest {
            superseded_by: a2.change_id.clone(),
            ..transition(&repo.id, &a.change_id, State::Superseded, false)
        })
        .await
        .unwrap();
    assert_eq!(
        superseded.operation.as_ref().unwrap().kind(),
        v1::OperationKind::Transition
    );
    let moved = &superseded.moved;
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_eq!(moved[0].change_id, b.change_id);
    assert!(moved[0].carried && !moved[0].conflicted, "{moved:?}");

    let b_now = change(client, &repo.id, &b.change_id).await;
    assert_eq!(b_now.parent_change_ids, std::slice::from_ref(&a2.change_id));
    let last = b_now.lifecycle.unwrap().last.unwrap();
    assert_eq!(last.cause(), TransitionCause::Dependency);
    let dependency = last.dependency.unwrap();
    assert_eq!(dependency.change_id, a2.change_id);
    assert_eq!(dependency.event(), DependencyEvent::Superseded);
    daemon.stop().await;
}

// Review finding 1: a declined change that a rewrite below it rebases is
// still declined: the changes above it stay held, and proposing the stack
// does not put it back.
#[tokio::test(flavor = "multi_thread")]
async fn a_rebased_decline_still_holds() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo).await;
    let z = commit_on(client, &repo.id, &main.commit_id, &[("z.txt", "z\n")]).await;
    let a = commit_on(client, &repo.id, &z.commit_id, &[("a.txt", "a\n")]).await;
    let b = commit_on(client, &repo.id, &a.commit_id, &[("b.txt", "b\n")]).await;
    client
        .transition_change(transition(&repo.id, &b.change_id, State::Proposed, true))
        .await
        .unwrap();
    client
        .transition_change(transition(&repo.id, &a.change_id, State::Declined, false))
        .await
        .unwrap();
    // Z is amended: A and B are rebased, their own diffs unchanged.
    client
        .edit_files(EditFilesRequest {
            repo: repo.id.clone(),
            target: Some(edit_files_request::Target::Revision(z.commit_id.clone())),
            edits: vec![FileEdit {
                path: "z2.txt".to_owned(),
                action: Some(file_edit::Action::Content(b"z2\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap();
    // Z's agent puts its amended change forward again.
    client
        .transition_change(transition(&repo.id, &z.change_id, State::Proposed, false))
        .await
        .unwrap();

    let landing = client
        .land_change(LandChangeRequest {
            repo: repo.id.clone(),
            revision: b.change_id.clone(),
            ..LandChangeRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&landing).0, "DEPENDENCY_LEFT_REVIEW");
    let proposing = client
        .transition_change(transition(&repo.id, &b.change_id, State::Proposed, true))
        .await
        .unwrap_err();
    assert_eq!(reason(&proposing).0, "MEMBER_LEFT_REVIEW");
    let a_now = lifecycle(client, &repo.id, &a.change_id).await;
    assert_eq!(a_now.state(), State::Declined);
    assert!(
        conflicts(client, &repo.id)
            .await
            .iter()
            .any(
                |conflict| conflict.kind() == ConflictKind::MissingDependency
                    && conflict.change.as_ref().unwrap().change_id == b.change_id
            )
    );
    daemon.stop().await;
}

// Review finding 2: with two declined changes in a row below a proposal,
// the command moves the proposal past both, not onto the upper one.
#[tokio::test(flavor = "multi_thread")]
async fn moving_off_skips_every_declined_change() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo).await;
    let a = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let b = commit_on(client, &repo.id, &a.commit_id, &[("b.txt", "b\n")]).await;
    let c = commit_on(client, &repo.id, &b.commit_id, &[("c.txt", "c\n")]).await;
    client
        .transition_change(transition(&repo.id, &c.change_id, State::Proposed, true))
        .await
        .unwrap();
    for declined in [&a, &b] {
        client
            .transition_change(transition(
                &repo.id,
                &declined.change_id,
                State::Declined,
                false,
            ))
            .await
            .unwrap();
    }
    let found = conflicts(client, &repo.id).await;
    let missing = found
        .iter()
        .find(|conflict| conflict.change.as_ref().unwrap().change_id == c.change_id)
        .and_then(|conflict| conflict.dependency.clone())
        .expect("C is held");
    assert_eq!(missing.change_id, a.change_id);
    assert_eq!(missing.move_change_id, c.change_id);
    assert_eq!(missing.move_onto, "main");
    daemon.stop().await;
}

/// `count` proposals on `on`, each then edited by its own agent: amended,
/// so the carry follower can never carry them.
async fn amended_by_their_owners(client: &Client, repo: &str, on: &str, count: usize) {
    for i in 0..count {
        let path = format!("j{i}.txt");
        let own = commit_on(client, repo, on, &[(&path, "j\n")]).await;
        client
            .transition_change(transition(repo, &own.change_id, State::Proposed, false))
            .await
            .unwrap();
        client
            .edit_files(EditFilesRequest {
                repo: repo.to_owned(),
                target: Some(edit_files_request::Target::Revision(own.commit_id.clone())),
                edits: vec![FileEdit {
                    path,
                    action: Some(file_edit::Action::Content(b"j2\n".to_vec())),
                    executable: None,
                }],
                ..EditFilesRequest::default()
            })
            .await
            .unwrap();
    }
}

/// Amends `commit` with a new file `path`, rebasing what is above it.
async fn amend(client: &Client, repo: &str, commit: &str, path: &str) {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(edit_files_request::Target::Revision(commit.to_owned())),
            edits: vec![FileEdit {
                path: path.to_owned(),
                action: Some(file_edit::Action::Content(b"amended\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap();
}

/// Z, A on Z, B on A: B proposed as a stack, A declined.
async fn declined_in_the_middle(
    client: &Client,
    repo: &SyntheticRepo,
) -> (v1::Change, v1::Change, v1::Change, v1::Change) {
    let main = setup(client, repo).await;
    let z = commit_on(client, &repo.id, &main.commit_id, &[("z.txt", "z\n")]).await;
    let a = commit_on(client, &repo.id, &z.commit_id, &[("a.txt", "a\n")]).await;
    let b = commit_on(client, &repo.id, &a.commit_id, &[("b.txt", "b\n")]).await;
    client
        .transition_change(transition(&repo.id, &b.change_id, State::Proposed, true))
        .await
        .unwrap();
    client
        .transition_change(transition(&repo.id, &a.change_id, State::Declined, false))
        .await
        .unwrap();
    (main, z, a, b)
}

// R3.11 second review, finding 3: more proposals amended by their own
// agents than one look checks, all newer than a declined change, do not
// let a stack proposal put the declined change back in review once a
// rewrite below it moved it, nor let its dependent land on it.
#[tokio::test(flavor = "multi_thread")]
async fn a_declined_change_stays_out_behind_proposals_amended_by_their_owners() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (main, z, a, b) = declined_in_the_middle(client, &repo).await;
    amended_by_their_owners(client, &repo.id, &main.commit_id, 64).await;
    // Z's owner amends it and proposes it again.
    amend(client, &repo.id, &z.commit_id, "z2.txt").await;
    client
        .transition_change(transition(&repo.id, &z.change_id, State::Proposed, false))
        .await
        .unwrap();

    let proposing = client
        .transition_change(transition(&repo.id, &b.change_id, State::Proposed, true))
        .await
        .unwrap_err();
    assert_eq!(reason(&proposing).0, "MEMBER_LEFT_REVIEW");
    assert_eq!(
        lifecycle(client, &repo.id, &a.change_id).await.state(),
        State::Declined
    );
    let landing = client
        .land_change(LandChangeRequest {
            repo: repo.id.clone(),
            revision: b.change_id.clone(),
            ..LandChangeRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&landing).0, "DEPENDENCY_LEFT_REVIEW");
    daemon.stop().await;
}

// R3.11 second review, finding 3: a carry check that cannot carry is made
// once, so the next look reaches the proposals behind it.
#[tokio::test(flavor = "multi_thread")]
async fn carry_checks_that_cannot_carry_are_made_once() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (main, z, a, b) = declined_in_the_middle(client, &repo).await;
    amended_by_their_owners(client, &repo.id, &main.commit_id, 64).await;
    amend(client, &repo.id, &z.commit_id, "z2.txt").await;

    let state = std::sync::Arc::downgrade(daemon.handle.state());
    let id = repo.id.parse().unwrap();
    for _ in 0..2 {
        reef_daemon::lifecycle::carry::carry_now(&state, id)
            .await
            .unwrap();
    }
    let a_now = change(client, &repo.id, &a.change_id).await;
    assert_eq!(
        a_now.lifecycle.unwrap().commit_id,
        a_now.commit_id,
        "A is carried"
    );
    assert!(
        !lifecycle(client, &repo.id, &b.change_id).await.amended,
        "B is carried"
    );
    daemon.stop().await;
}

// R3.11 second review, finding 2: garbage collection that takes one
// proposal's recorded commit stops neither the other carries nor the
// missing-dependency reads.
#[tokio::test(flavor = "multi_thread")]
async fn a_collected_commit_does_not_stop_carries_or_holds() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        config.file.gc.undo_window = Duration::ZERO;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    let client = &daemon.client;
    let main = setup(client, &repo).await;
    let a = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let b = commit_on(client, &repo.id, &a.commit_id, &[("b.txt", "b\n")]).await;
    client
        .transition_change(transition(&repo.id, &b.change_id, State::Proposed, true))
        .await
        .unwrap();
    client
        .transition_change(transition(&repo.id, &a.change_id, State::Declined, false))
        .await
        .unwrap();
    let x = commit_on(client, &repo.id, &main.commit_id, &[("x.txt", "x\n")]).await;
    let y = commit_on(client, &repo.id, &x.commit_id, &[("y.txt", "y\n")]).await;
    client
        .transition_change(transition(&repo.id, &y.change_id, State::Proposed, false))
        .await
        .unwrap();
    // W, withdrawn and then revised by its owner: its recorded commit is
    // garbage once the undo window is over.
    let revised = commit_on(client, &repo.id, &main.commit_id, &[("w.txt", "w\n")]).await;
    client
        .transition_change(transition(
            &repo.id,
            &revised.change_id,
            State::Proposed,
            false,
        ))
        .await
        .unwrap();
    client
        .transition_change(transition(
            &repo.id,
            &revised.change_id,
            State::Withdrawn,
            false,
        ))
        .await
        .unwrap();
    amend(client, &repo.id, &revised.commit_id, "w2.txt").await;
    let held = async |client: &Client| {
        conflicts(client, &repo.id)
            .await
            .iter()
            .filter(|conflict| conflict.kind() == ConflictKind::MissingDependency)
            .count()
    };
    assert_eq!(held(client).await, 1, "B is held by A");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    client.collect_garbage(&repo.id, false).await.unwrap();
    // A new daemon has checked nothing yet.
    let daemon = daemon.restart().await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    let client = &daemon.client;
    assert_eq!(held(client).await, 1, "B is still held by A");

    amend(client, &repo.id, &x.change_id, "x2.txt").await;
    eventually("Y is carried across X's amend", || async {
        !lifecycle(client, &repo.id, &y.change_id).await.amended
    })
    .await;
    daemon.stop().await;
}

// R3.11 second review, findings 4 and 7: a supersede that moved its
// dependents can be undone, in one operation that puts them back; its
// carries name the successor, for its children's children too.
#[tokio::test(flavor = "multi_thread")]
async fn a_supersede_that_moved_dependents_is_undone() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo).await;
    let a = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let b = commit_on(client, &repo.id, &a.commit_id, &[("b.txt", "b\n")]).await;
    let c = commit_on(client, &repo.id, &b.commit_id, &[("c.txt", "c\n")]).await;
    let a2 = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a2\n")]).await;
    client
        .transition_change(transition(&repo.id, &c.change_id, State::Proposed, true))
        .await
        .unwrap();
    let superseded = client
        .transition_change(TransitionChangeRequest {
            superseded_by: a2.change_id.clone(),
            ..transition(&repo.id, &a.change_id, State::Superseded, false)
        })
        .await
        .unwrap();
    for carried in [&b, &c] {
        let last = lifecycle(client, &repo.id, &carried.change_id)
            .await
            .last
            .unwrap();
        let dependency = last.dependency.clone().unwrap();
        assert_eq!(dependency.change_id, a2.change_id, "{last:?}");
        assert_eq!(dependency.event(), DependencyEvent::Superseded);
    }

    let op = superseded.operation.unwrap().id;
    client
        .undo_operation(v1::UndoOperationRequest {
            repo: repo.id.clone(),
            operation_id: op,
            ..v1::UndoOperationRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(
        lifecycle(client, &repo.id, &a.change_id).await.state(),
        State::Proposed
    );
    assert_eq!(
        change(client, &repo.id, &b.change_id)
            .await
            .parent_change_ids,
        vec![a.change_id.clone()]
    );
    daemon.stop().await;
}

// R3.11 second review, finding 5: a dependent whose stack cannot be read
// (its destination is gone) keeps its dependency assignment open.
#[tokio::test(flavor = "multi_thread")]
async fn an_unread_stack_closes_no_assignment() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo).await;
    let a = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let b = commit_on(client, &repo.id, &a.commit_id, &[("b.txt", "b\n")]).await;
    client
        .transition_change(transition(&repo.id, &b.change_id, State::Proposed, true))
        .await
        .unwrap();
    client
        .transition_change(transition(&repo.id, &a.change_id, State::Declined, false))
        .await
        .unwrap();
    let open = async || {
        conflicts(client, &repo.id).await;
        assignments(client, &repo.id)
            .await
            .into_iter()
            .filter(|row| {
                row.kind() == ConflictKind::MissingDependency
                    && row.state() != v1::ConflictAssignmentState::Closed
            })
            .count()
    };
    assert_eq!(open().await, 1, "B's dependency assignment is open");
    delete_bookmark(&repo, "main").await;
    // Another operation, so the daemon reads the repository again.
    commit_on(client, &repo.id, &main.commit_id, &[("o.txt", "o\n")]).await;
    assert_eq!(
        open().await,
        1,
        "a stack that cannot be read closes nothing"
    );
    daemon.stop().await;
}

// R3.11 second review, finding 6: a carry of a held dependent, or of the
// declined change below it, is not a new decline: its agent is not told
// again, and the conflict is the same.
#[tokio::test(flavor = "multi_thread")]
async fn a_carry_does_not_repeat_the_missing_dependency_instruction() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (_, z, _, b) = declined_in_the_middle(client, &repo).await;
    eventually("B is told once", || async {
        instructions(client, &repo.id, &b.change_id).await.len() == 1
    })
    .await;
    // Its agent finished with it.
    let told = instructions(client, &repo.id, &b.change_id).await;
    client
        .update_instruction(v1::UpdateInstructionRequest {
            repo: repo.id.clone(),
            id: told[0].id.clone(),
            to: v1::InstructionState::Cancelled.into(),
            ..v1::UpdateInstructionRequest::default()
        })
        .await
        .unwrap();
    let missing_id = async || {
        conflicts(client, &repo.id)
            .await
            .into_iter()
            .find(|conflict| conflict.kind() == ConflictKind::MissingDependency)
            .map(|conflict| conflict.id)
    };
    let before = missing_id().await;
    let carried = lifecycle(client, &repo.id, &b.change_id).await.commit_id;
    amend(client, &repo.id, &z.commit_id, "z2.txt").await;
    eventually("B is carried", || async {
        let now = lifecycle(client, &repo.id, &b.change_id).await;
        now.commit_id != carried && !now.amended
    })
    .await;
    // The follower looks again after the carry's own operation.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(instructions(client, &repo.id, &b.change_id).await.len(), 1);
    assert!(before.is_some());
    assert_eq!(missing_id().await, before);
    daemon.stop().await;
}

// R3.11 second review, F3: a declined change that becomes divergent still
// holds the proposal above it: its missing dependency and assignment stay.
#[tokio::test(flavor = "multi_thread")]
async fn a_divergent_declined_change_still_holds() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(client, &repo).await;
    let a = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    let b = commit_on(client, &repo.id, &a.commit_id, &[("b.txt", "b\n")]).await;
    client
        .transition_change(transition(&repo.id, &b.change_id, State::Proposed, true))
        .await
        .unwrap();
    client
        .transition_change(transition(&repo.id, &a.change_id, State::Declined, false))
        .await
        .unwrap();
    let held = async || {
        let missing = conflicts(client, &repo.id)
            .await
            .iter()
            .filter(|conflict| conflict.kind() == ConflictKind::MissingDependency)
            .count();
        let open = assignments(client, &repo.id)
            .await
            .into_iter()
            .filter(|row| {
                row.kind() == ConflictKind::MissingDependency
                    && row.state() != v1::ConflictAssignmentState::Closed
            })
            .count();
        (missing, open)
    };
    assert_eq!(held().await, (1, 1));
    duplicate_change(&repo, &a.commit_id).await;
    // Another operation, so the daemon reads the repository again.
    commit_on(client, &repo.id, &main.commit_id, &[("o.txt", "o\n")]).await;
    assert_eq!(held().await, (1, 1), "B is still held by A");
    daemon.stop().await;
}
