//! Issue #248: a workspace whose commit has landed never amends it. The
//! observer gives such a workspace a new empty change on the landed commit
//! (in the landing's operation for a recorded change), and a snapshot never
//! rewrites a commit on `trunk()` or under a tag.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, CreateWorkspaceRequest, EditFilesRequest, FileEdit, GetChangeRequest,
    GetWorkspaceRequest, LifecycleState as State, OperationKind, TransitionCause,
    TransitionChangeRequest, edit_files_request, file_edit,
};
use reef_daemon::client::Client;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn change_at(client: &Client, repo: &str, workspace: &str, revision: &str) -> v1::Change {
    client
        .get_change(GetChangeRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            revision: revision.to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

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

/// Sets a local bookmark, or a tag, from outside the daemon, as the jj CLI
/// would.
fn set_ref(root: &Path, tag: bool, name: &str, commit: &str) {
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
    let target = RefTarget::normal(CommitId::try_from_hex(commit).unwrap());
    if tag {
        tx.repo_mut()
            .set_local_tag_target(RefName::new(name), target);
    } else {
        tx.repo_mut()
            .set_local_bookmark_target(RefName::new(name), target);
    }
    pollster::block_on(tx.commit("ref set")).unwrap();
}

async fn bookmark(repo: &SyntheticRepo, name: &str, commit: &str) {
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || set_ref(&root, false, &name, &commit))
        .await
        .unwrap();
}

async fn tag(repo: &SyntheticRepo, name: &str, commit: &str) {
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || set_ref(&root, true, &name, &commit))
        .await
        .unwrap();
}

/// The commit `main` points at.
async fn main_head(client: &Client, repo: &str) -> String {
    change_at(client, repo, "", "main").await.commit_id
}

/// `main` on a base commit, and a workspace on a new change on it, with
/// `w.txt` written and snapshotted.
async fn setup(daemon: &TestDaemon, repo: &SyntheticRepo) -> (v1::Change, v1::Workspace) {
    let client = &daemon.client;
    let main = commit_on(client, &repo.id, "root()", &[("README.md", "base\n")]).await;
    bookmark(repo, "main", &main.commit_id).await;
    let ws = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            start: Some(v1::create_workspace_request::Start::NewOn(
                main.commit_id.clone(),
            )),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    std::fs::write(Path::new(&ws.path).join("w.txt"), "w\n").unwrap();
    client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    (main, ws)
}

/// Reads the workspace's `@` until its change is no longer `change`, for at
/// most 10 seconds; each read refreshes the repository, which wakes the
/// observer.
async fn wait_for_new_change(
    client: &Client,
    repo: &str,
    workspace: &str,
    change: &str,
) -> v1::Change {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let at = change_at(client, repo, workspace, "@").await;
        if at.change_id != change {
            return at;
        }
        assert!(
            Instant::now() < deadline,
            "the workspace never left change {change}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn operation(client: &Client, repo: &str, id: &str) -> v1::Operation {
    client
        .get_operation(repo, id)
        .await
        .unwrap()
        .operation
        .unwrap()
}

// 21a, 21b. A proposed change on a workspace's `@` reaches `main` by an
// external bookmark move: one operation records LANDED and gives the
// workspace a new empty change on it. An edit on disk at the move, not yet
// snapshotted, goes into the new change; `main` never moves.
#[tokio::test(flavor = "multi_thread")]
async fn observed_landing_moves_the_workspace() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (_, ws) = setup(&daemon, &repo).await;
    let proposed = client
        .transition_change(TransitionChangeRequest {
            repo: repo.id.clone(),
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            to: State::Proposed.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
    let landed_commit = proposed.change.unwrap().commit_id;
    let landed_change = change_at(client, &repo.id, &ws.id, "@").await.change_id;
    // An edit not yet snapshotted when main moves.
    std::fs::write(Path::new(&ws.path).join("late.txt"), "late\n").unwrap();
    bookmark(&repo, "main", &landed_commit).await;

    let at = wait_for_new_change(client, &repo.id, &ws.id, &landed_change).await;
    assert_eq!(at.parent_commit_ids, std::slice::from_ref(&landed_commit));
    assert!(at.empty, "{at:?}");
    let landed = change_at(client, &repo.id, "", &landed_commit).await;
    let last = landed.lifecycle.unwrap().last.unwrap();
    assert_eq!(last.to(), State::Landed);
    assert_eq!(last.cause(), TransitionCause::Observed);
    // The landing's operation is the one that moved the workspace.
    let op = operation(client, &repo.id, &last.operation_id).await;
    assert_eq!(op.kind(), OperationKind::Transition);
    let moved = client
        .get_workspace(GetWorkspaceRequest {
            repo: repo.id.clone(),
            workspace: ws.id.clone(),
            skip_disk_usage: true,
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(moved.commit_id, at.commit_id);

    // The late edit goes into the new change; main stays.
    client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert_eq!(main_head(client, &repo.id).await, landed_commit);
    let after = change_at(client, &repo.id, &ws.id, "@").await;
    assert_eq!(after.change_id, at.change_id);
    assert_eq!(
        after.parent_commit_ids,
        std::slice::from_ref(&landed_commit)
    );
    assert!(!after.empty);
    let landed_again = change_at(client, &repo.id, "", &landed_commit).await;
    assert_eq!(
        landed_again.commit_id, landed_commit,
        "the landed commit was rewritten"
    );
}

// 21c. A change never proposed reaches `main`: the workspace gets a new
// change in an operation of its own.
#[tokio::test(flavor = "multi_thread")]
async fn unrecorded_landing_moves_the_workspace() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (_, ws) = setup(&daemon, &repo).await;
    let before = change_at(client, &repo.id, &ws.id, "@").await;
    bookmark(&repo, "main", &before.commit_id).await;
    let at = wait_for_new_change(client, &repo.id, &ws.id, &before.change_id).await;
    assert_eq!(
        at.parent_commit_ids,
        std::slice::from_ref(&before.commit_id)
    );
    assert!(at.empty);
    std::fs::write(Path::new(&ws.path).join("w.txt"), "w2\n").unwrap();
    client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert_eq!(main_head(client, &repo.id).await, before.commit_id);
}

// 21d. The backstop: a tag on the workspace's commit (the observer acts
// only when trunk moves). An edit and a snapshot start a new change on the
// tagged commit, which is not rewritten.
#[tokio::test(flavor = "multi_thread")]
async fn snapshot_starts_a_new_change_on_a_tagged_commit() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (_, ws) = setup(&daemon, &repo).await;
    let before = change_at(client, &repo.id, &ws.id, "@").await;
    tag(&repo, "v1", &before.commit_id).await;
    std::fs::write(Path::new(&ws.path).join("w.txt"), "w2\n").unwrap();
    let snapshot = client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let after = change_at(client, &repo.id, &ws.id, "@").await;
    assert_ne!(after.change_id, before.change_id);
    assert_eq!(
        after.parent_commit_ids,
        std::slice::from_ref(&before.commit_id)
    );
    assert!(!after.empty);
    let tagged = change_at(client, &repo.id, "", "v1").await;
    assert_eq!(tagged.commit_id, before.commit_id);
    let op = operation(client, &repo.id, &snapshot.operation.unwrap().id).await;
    assert!(
        op.description.contains("new change on") && op.description.contains("tag v1"),
        "{}",
        op.description
    );
}
