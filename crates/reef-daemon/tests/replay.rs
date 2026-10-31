//! Replay (R2.12, tests 4 to 12): status, workspaces and the operation log
//! read at an operation through the daemon, operations found by time, and
//! reads at an operation across GC.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, SystemTime};

use reef_api::v1::{
    self, CreateWorkspaceRequest, GetOperationRequest, GetStatusRequest, GetWorkspaceRequest,
    ListChangesRequest, ListOperationsRequest, ListWorkspacesRequest, StatusSource, WorkspaceState,
    create_workspace_request,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, test_settings};
use reef_lib::jj::Repo;
use reef_testing::synth::Preset;
use tonic::Code;

/// A daemon whose watcher never snapshots on its own, over a small
/// synthetic repository whose snapshot commit is bookmarked `main`.
async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
}

/// [`fixture`] with the configuration changed by `configure` too.
async fn fixture_with(
    configure: impl FnOnce(&mut reef_daemon::config::Config),
) -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
        configure(config);
    })
    .await;
    let commit = Repo::open(&repo.root, &test_settings())
        .await
        .unwrap()
        .wc_change()
        .unwrap()
        .commit_id;
    set_bookmark(&repo.root, "main", commit.as_str());
    (daemon, repo)
}

/// Sets bookmark `name` to `commit` in a jj transaction from outside the
/// daemon, as a user's `jj` would.
fn set_bookmark(root: &Path, name: &str, commit: &str) {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;
    use jj_lib::workspace::Workspace;
    let settings = test_settings();
    let workspace = Workspace::load(
        &settings,
        root,
        &default_backend_factories(),
        &default_working_copy_factories(),
    )
    .unwrap();
    let repo = pollster::block_on(workspace.repo_loader().load_at_head()).unwrap();
    let mut tx = repo.start_transaction();
    tx.repo_mut().set_local_bookmark_target(
        RefName::new(name),
        RefTarget::normal(CommitId::try_from_hex(commit).unwrap()),
    );
    pollster::block_on(tx.commit("bookmark set")).unwrap();
}

async fn create(
    client: &Client,
    repo: &SyntheticRepo,
    owner: &str,
    start: Option<create_workspace_request::Start>,
) -> v1::Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: owner.to_owned(),
            start,
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

fn write(workspace: &v1::Workspace, path: &str, contents: &str) {
    let path = Path::new(&workspace.path).join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Snapshots `workspace` and returns it afterwards.
async fn snapshot(
    client: &Client,
    repo: &SyntheticRepo,
    workspace: &v1::Workspace,
) -> v1::Workspace {
    client
        .snapshot_workspace(&repo.id, &workspace.id, "")
        .await
        .unwrap();
    get(client, repo, &workspace.id, "")
        .await
        .workspace
        .unwrap()
}

async fn get(
    client: &Client,
    repo: &SyntheticRepo,
    workspace: &str,
    at_operation: &str,
) -> v1::GetWorkspaceResponse {
    client
        .get_workspace(GetWorkspaceRequest {
            repo: repo.id.clone(),
            workspace: workspace.to_owned(),
            skip_disk_usage: true,
            at_operation: at_operation.to_owned(),
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
}

/// Every operation, newest first.
async fn operations(client: &Client, repo: &SyntheticRepo) -> Vec<v1::Operation> {
    client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            page_size: 1000,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations
}

async fn head_op(client: &Client, repo: &SyntheticRepo) -> String {
    operations(client, repo).await[0].id.clone()
}

async fn workspaces_at(client: &Client, repo: &SyntheticRepo, at: &str) -> Vec<v1::Workspace> {
    client
        .list_workspaces(ListWorkspacesRequest {
            repo: repo.id.clone(),
            at_operation: at.to_owned(),
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap()
}

async fn status_at(
    client: &Client,
    repo: &SyntheticRepo,
    workspace: &str,
    at: &str,
) -> Result<v1::GetStatusResponse, ClientError> {
    client
        .get_status(GetStatusRequest {
            repo: repo.id.clone(),
            workspace: workspace.to_owned(),
            at_operation: at.to_owned(),
            ..GetStatusRequest::default()
        })
        .await
}

fn rpc(error: &ClientError) -> (Code, &str, &std::collections::BTreeMap<String, String>) {
    match error {
        ClientError::Rpc {
            status,
            reason,
            metadata,
            ..
        } => (status.code(), reason.as_str(), metadata),
        other => panic!("{other:?}"),
    }
}

fn ids(workspaces: &[v1::Workspace]) -> BTreeSet<String> {
    workspaces.iter().map(|w| w.id.clone()).collect()
}

/// Commit by workspace id.
fn commits(workspaces: &[v1::Workspace]) -> BTreeSet<(String, String)> {
    workspaces
        .iter()
        .map(|w| (w.id.clone(), w.commit_id.clone()))
        .collect()
}

fn system_time(timestamp: &prost_types::Timestamp) -> SystemTime {
    SystemTime::UNIX_EPOCH
        + Duration::from_secs(u64::try_from(timestamp.seconds).unwrap())
        + Duration::from_nanos(u64::try_from(timestamp.nanos).unwrap())
}

/// The directory of jj's index links for `root`'s repository.
fn op_links(root: &Path) -> BTreeSet<String> {
    std::fs::read_dir(root.join(".jj/repo/index/op_links"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn status_at_operation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    let before_v = head_op(client, &repo).await;
    write(&w, "w.txt", "w\n");
    let w = snapshot(client, &repo, &w).await;
    let v = create(client, &repo, "v@test", None).await;
    write(&v, "v.txt", "v\n");
    let v = snapshot(client, &repo, &v).await;
    let x = head_op(client, &repo).await;
    let trunk_then = status_at(client, &repo, "", "")
        .await
        .unwrap()
        .repository_status
        .unwrap()
        .trunk
        .unwrap()
        .commit_id;

    client
        .release_workspace(&repo.id, &v.id, true, false)
        .await
        .unwrap();
    write(&w, "w.txt", "w later\n");
    snapshot(client, &repo, &w).await;
    set_bookmark(&repo.root, "main", &w.commit_id);

    let response = status_at(client, &repo, "", &x[..12]).await.unwrap();
    assert_eq!(response.at_operation, x);
    let state = response.repository_status.unwrap();
    assert_eq!(state.active_workspace_count, 2);
    assert_eq!(
        commits(&state.active_workspaces),
        commits(&[w.clone(), v.clone()])
    );
    assert_eq!(state.trunk.unwrap().commit_id, trunk_then);
    assert_eq!(state.last_operation.unwrap().id, x);
    // Now, the release and the bookmark show.
    let now = status_at(client, &repo, "", "").await.unwrap();
    let now = now.repository_status.unwrap();
    assert_eq!(now.active_workspace_count, 1);
    assert_eq!(now.trunk.unwrap().commit_id, w.commit_id);

    // The released workspace, named, answers as it was at X.
    let response = status_at(client, &repo, &v.id, &x).await.unwrap();
    let found = response.workspace_status.unwrap();
    assert_eq!(found.source(), StatusSource::Operation);
    assert_eq!(found.working_copy.unwrap().commit_id, v.commit_id);
    assert!(found.snapshot.is_none());
    assert_eq!(found.workspace.unwrap().state(), WorkspaceState::Active);
    assert!(
        found.changed_files.iter().any(|file| file.path == "v.txt"),
        "{:?}",
        found.changed_files
    );

    // Before V existed.
    let error = status_at(client, &repo, &v.id, &before_v)
        .await
        .unwrap_err();
    let (code, reason, metadata) = rpc(&error);
    assert_eq!((code, reason), (Code::NotFound, "WORKING_COPY_NOT_FOUND"));
    assert_eq!(metadata["workspace_id"], v.id);
    assert_eq!(metadata["at_operation"], before_v);

    // Nothing is recorded, even with edits on disk and no skip_snapshot.
    write(&w, "unsaved.txt", "unsaved\n");
    let count = operations(client, &repo).await.len();
    status_at(client, &repo, &w.id, &x).await.unwrap();
    assert_eq!(operations(client, &repo).await.len(), count);
    daemon.stop().await;
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn workspaces_at_operation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "a.txt", "a\n");
    let w1 = snapshot(client, &repo, &w).await;
    let x1 = head_op(client, &repo).await;
    write(&w, "a.txt", "b\n");
    let w2 = snapshot(client, &repo, &w).await;
    assert_ne!(w1.commit_id, w2.commit_id);

    let then = workspaces_at(client, &repo, &x1).await;
    assert_eq!(then.len(), 1);
    let found = &then[0];
    assert_eq!(found.commit_id, w1.commit_id);
    assert_eq!(found.change_id, w1.change_id);
    assert_eq!(found.owner, "w@test");
    assert_eq!(
        found.last_snapshot_operation_id,
        w1.last_snapshot_operation_id
    );
    assert_eq!(found.local_snapshot_count, w1.local_snapshot_count);
    assert!(found.local_snapshot_count < w2.local_snapshot_count);
    let lease = found.lease.clone().unwrap();
    assert_eq!(lease.owner, "w@test");
    assert!(lease.issued_at.is_some());
    assert!(lease.expires_at.is_none() && lease.last_heartbeat_at.is_none());
    assert_eq!(lease.state(), v1::LeaseState::Unspecified);
    assert_eq!(lease.end_reason(), v1::LeaseEndReason::Unspecified);
    assert_eq!(found.attention(), v1::Attention::Unspecified);
    assert_eq!(found.watch_state(), v1::WatchState::Unspecified);
    assert_eq!(found.stores, []);

    // Changed files as of then: W's change had added a.txt.
    let counted = client
        .list_workspaces(ListWorkspacesRequest {
            repo: repo.id.clone(),
            at_operation: x1.clone(),
            changed_file_counts: true,
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(counted[0].changed_file_count, Some(1));

    // Filters.
    let filtered = |states: Vec<i32>, owner: &str| ListWorkspacesRequest {
        repo: repo.id.clone(),
        states,
        owner: owner.to_owned(),
        at_operation: x1.clone(),
        ..ListWorkspacesRequest::default()
    };
    let released = client
        .list_workspaces(filtered(vec![WorkspaceState::Released.into()], ""))
        .await
        .unwrap();
    assert_eq!(released, []);
    let nobody = client
        .list_workspaces(filtered(Vec::new(), "nobody"))
        .await
        .unwrap();
    assert_eq!(nobody, []);
    assert_eq!(
        client
            .list_workspaces(filtered(Vec::new(), "w@test"))
            .await
            .unwrap()
            .len(),
        1
    );

    // Pages read the same point: a workspace created after it never shows.
    create(client, &repo, "u@test", None).await;
    let x2 = head_op(client, &repo).await;
    create(client, &repo, "t@test", None).await;
    let mut request = ListWorkspacesRequest {
        repo: repo.id.clone(),
        at_operation: x2.clone(),
        page_size: 1,
        ..ListWorkspacesRequest::default()
    };
    let mut owners = Vec::new();
    loop {
        let page = client.list_workspaces_page(request.clone()).await.unwrap();
        assert_eq!(page.at_operation, x2);
        owners.extend(page.workspaces.iter().map(|w| w.owner.clone()));
        if page.next_page_token.is_empty() {
            break;
        }
        request.page_token = page.next_page_token;
    }
    assert_eq!(owners, ["u@test", "w@test"]);
    assert_eq!(workspaces_at(client, &repo, "").await.len(), 3);

    // No repository.
    let error = client
        .list_workspaces_page(ListWorkspacesRequest {
            at_operation: x2.clone(),
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(rpc(&error).0, Code::InvalidArgument);
    daemon.stop().await;
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn get_workspace_at_operation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    let before = head_op(client, &repo).await;
    write(&w, "a.txt", "a\n");
    let w = snapshot(client, &repo, &w).await;
    let v = create(
        client,
        &repo,
        "v@test",
        Some(create_workspace_request::Start::Change(w.change_id.clone())),
    )
    .await;
    let x = head_op(client, &repo).await;
    write(&w, "a.txt", "b\n");
    snapshot(client, &repo, &w).await;

    let then = get(client, &repo, &w.id, &x).await;
    assert_eq!(then.at_operation, x);
    let workspace = then.workspace.unwrap();
    assert_eq!(workspace.commit_id, w.commit_id);
    assert_eq!(then.other_workspaces_on_change, std::slice::from_ref(&v.id));
    assert!(then.disk_usage.is_none() && then.snapshot.is_none());
    assert_eq!(then.snapshot_count, 1);
    assert_eq!(then.recent_snapshots.len(), 1);
    assert_eq!(get(client, &repo, &w.id, "").await.snapshot_count, 2);

    let error = client
        .get_workspace(GetWorkspaceRequest {
            repo: repo.id.clone(),
            workspace: w.id.clone(),
            snapshot_first: true,
            at_operation: x.clone(),
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(rpc(&error).0, Code::InvalidArgument);
    let error = client
        .get_workspace(GetWorkspaceRequest {
            repo: repo.id.clone(),
            workspace: v.id.clone(),
            at_operation: before.clone(),
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        (rpc(&error).0, rpc(&error).1),
        (Code::NotFound, "WORKING_COPY_NOT_FOUND")
    );
    daemon.stop().await;
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn list_operations_at_operation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    for n in 0..5 {
        write(&w, "a.txt", &format!("{n}\n"));
        snapshot(client, &repo, &w).await;
    }
    let all = operations(client, &repo).await;
    let x = all[3].id.clone();
    let mut request = ListOperationsRequest {
        repo: repo.id.clone(),
        at_operation: x[..12].to_owned(),
        page_size: 2,
        ..ListOperationsRequest::default()
    };
    let mut listed = Vec::new();
    loop {
        let page = client.list_operations_page(request.clone()).await.unwrap();
        assert_eq!(page.at_operation, x);
        listed.extend(page.operations.into_iter().map(|op| op.id));
        if page.next_page_token.is_empty() {
            break;
        }
        request.page_token = page.next_page_token;
    }
    let expected: Vec<String> = all[3..].iter().map(|op| op.id.clone()).collect();
    assert_eq!(listed, expected);
    // `ended_after` stops it there too.
    let page = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            at_operation: x.clone(),
            ended_after: all[5].ended_at,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap();
    let ended: Vec<String> = page.operations.into_iter().map(|op| op.id).collect();
    assert!(ended.len() <= 2 && ended.first() == Some(&x), "{ended:?}");
    daemon.stop().await;
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn locate_by_time() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    for n in 0..4 {
        write(&w, "a.txt", &format!("{n}\n"));
        snapshot(client, &repo, &w).await;
    }
    let all = operations(client, &repo).await;
    let target = &all[2];
    let found = client
        .get_operation_with(GetOperationRequest {
            repo: repo.id.clone(),
            ended_at_or_before: target.ended_at,
            ..GetOperationRequest::default()
        })
        .await
        .unwrap()
        .operation
        .unwrap();
    // The newest that ended at or before the time, in log order.
    let at = system_time(target.ended_at.as_ref().unwrap());
    let expected = all
        .iter()
        .find(|op| system_time(op.ended_at.as_ref().unwrap()) <= at)
        .unwrap();
    assert_eq!(found.id, expected.id);
    assert!(found.effects.is_some());
    // From a given operation.
    let found = client
        .get_operation_with(GetOperationRequest {
            repo: repo.id.clone(),
            operation_id: all[3].id.clone(),
            ended_at_or_before: Some(prost_types::Timestamp::from(SystemTime::now())),
        })
        .await
        .unwrap()
        .operation
        .unwrap();
    assert_eq!(found.id, all[3].id);
    // Before the repository's first operation.
    let error = client
        .get_operation_with(GetOperationRequest {
            repo: repo.id.clone(),
            ended_at_or_before: Some(prost_types::Timestamp::from(
                SystemTime::now() - Duration::from_hours(24 * 365),
            )),
            ..GetOperationRequest::default()
        })
        .await
        .unwrap_err();
    let (code, reason, metadata) = rpc(&error);
    assert_eq!(
        (code, reason),
        (Code::FailedPrecondition, "OPERATION_NOT_RETAINED")
    );
    assert!(metadata.contains_key("oldest_at"));
    // Neither field.
    let error = client
        .get_operation_with(GetOperationRequest {
            repo: repo.id.clone(),
            ..GetOperationRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(rpc(&error).1, "INVALID_OPERATION_ID");
    // A budget that runs out.
    reef_daemon::service::at_operation::LOCATE_BUDGET
        .store(2, std::sync::atomic::Ordering::Relaxed);
    let error = client
        .get_operation_with(GetOperationRequest {
            repo: repo.id.clone(),
            ended_at_or_before: all[5].ended_at,
            ..GetOperationRequest::default()
        })
        .await
        .unwrap_err();
    let (code, reason, metadata) = rpc(&error);
    assert_eq!((code, reason), (Code::FailedPrecondition, "TOO_FAR_BACK"));
    assert_eq!(metadata["repo_id"], repo.id);
    assert!(metadata.contains_key("examined_to"));
    daemon.stop().await;
}

// 9
#[tokio::test(flavor = "multi_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "reads before and after one GC, by id, by time and by walk"
)]
async fn replay_across_gc() {
    let (daemon, repo) =
        fixture_with(|config| config.file.gc.undo_window = Duration::from_secs(10)).await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "old.txt", "old\n");
    snapshot(client, &repo, &w).await;
    // Everything so far falls outside the window; what follows inside it.
    // The window is wide enough that the reads below, slow under load, end
    // well inside it.
    tokio::time::sleep(Duration::from_millis(10_500)).await;
    let v = create(client, &repo, "v@test", None).await;
    write(&v, "v.txt", "v\n");
    snapshot(client, &repo, &v).await;
    let x = head_op(client, &repo).await;
    write(&w, "w.txt", "w\n");
    snapshot(client, &repo, &w).await;
    let state = |response: v1::GetStatusResponse| {
        let state = response.repository_status.unwrap();
        (
            commits(&state.active_workspaces),
            state.trunk.map(|trunk| trunk.commit_id),
            state
                .heads
                .into_iter()
                .map(|head| head.commit_id)
                .collect::<Vec<_>>(),
        )
    };
    let before = state(status_at(client, &repo, "", &x).await.unwrap());
    let changes = |at: String| {
        client.list_changes(ListChangesRequest {
            repo: repo.id.clone(),
            revset: "all()".to_owned(),
            at_operation: at,
            ..ListChangesRequest::default()
        })
    };
    let changes_before: Vec<String> = changes(x.clone())
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.change.unwrap().commit_id)
        .collect();

    let report = client
        .collect_garbage(&repo.id, false)
        .await
        .unwrap()
        .reports;
    assert_eq!(report[0].jj_error, "", "{report:?}");
    let links = op_links(&repo.root);
    // The pre-GC id still answers, the same, and builds no index.
    assert_eq!(
        state(status_at(client, &repo, "", &x).await.unwrap()),
        before
    );
    let changes_after: Vec<String> = changes(x.clone())
        .await
        .unwrap()
        .into_iter()
        .map(|entry| entry.change.unwrap().commit_id)
        .collect();
    assert_eq!(changes_after, changes_before);
    assert_eq!(commits(&workspaces_at(client, &repo, &x).await), before.0);
    assert_eq!(op_links(&repo.root), links);
    // Its rewrite, found by its time, answers the same.
    let ended = client
        .get_operation_with(GetOperationRequest {
            repo: repo.id.clone(),
            operation_id: x.clone(),
            ..GetOperationRequest::default()
        })
        .await
        .unwrap()
        .operation
        .unwrap()
        .ended_at;
    let rewrite = client
        .get_operation_with(GetOperationRequest {
            repo: repo.id.clone(),
            ended_at_or_before: ended,
            ..GetOperationRequest::default()
        })
        .await
        .unwrap()
        .operation
        .unwrap();
    assert_ne!(rewrite.id, x);
    assert_eq!(
        state(status_at(client, &repo, "", &rewrite.id).await.unwrap()),
        before
    );
    // A walk from the pre-GC id reaches what GC collected.
    let error = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            at_operation: x.clone(),
            page_size: 1000,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap_err();
    let (code, reason, metadata) = rpc(&error);
    assert_eq!(
        (code, reason),
        (Code::FailedPrecondition, "OPERATION_LOG_REWRITTEN")
    );
    assert_eq!(metadata["repo_id"], repo.id);
    // Before the oldest kept operation.
    let error = client
        .get_operation_with(GetOperationRequest {
            repo: repo.id.clone(),
            ended_at_or_before: Some(prost_types::Timestamp::from(
                SystemTime::now() - Duration::from_secs(3600),
            )),
            ..GetOperationRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(rpc(&error).1, "OPERATION_NOT_RETAINED");
    daemon.stop().await;
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn external_writer_replay() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "a.txt", "a\n");
    let w = snapshot(client, &repo, &w).await;
    let before = head_op(client, &repo).await;
    set_bookmark(&repo.root, "main", &w.commit_id);
    let all = operations(client, &repo).await;
    let external = &all[0];
    assert_eq!(external.kind(), v1::OperationKind::External);
    let trunk = |response: v1::GetStatusResponse| {
        response.repository_status.unwrap().trunk.unwrap().commit_id
    };
    assert_eq!(
        trunk(status_at(client, &repo, "", &external.id).await.unwrap()),
        w.commit_id
    );
    assert_ne!(
        trunk(status_at(client, &repo, "", &before).await.unwrap()),
        w.commit_id
    );
    assert_eq!(
        ids(&workspaces_at(client, &repo, &external.id).await),
        ids(&workspaces_at(client, &repo, &before).await)
    );
    daemon.stop().await;
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn replay_reads_are_cached() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    for n in 0..3 {
        write(&w, "a.txt", &format!("{n}\n"));
        snapshot(client, &repo, &w).await;
    }
    let all = operations(client, &repo).await;
    let reads = &daemon.handle.state().reads;
    let loads = reads.at_loads();
    for n in 0..20 {
        let at = &all[1 + n % 2].id;
        status_at(client, &repo, "", at).await.unwrap();
        workspaces_at(client, &repo, at).await;
    }
    assert_eq!(reads.at_loads() - loads, 2);
    // A scrubber stepping back over every operation and forward again
    // loads each once (the cache holds 64).
    let loads = reads.at_loads();
    let back: Vec<&v1::Operation> = all[1..].iter().collect();
    for op in back.iter().chain(back.iter().rev()) {
        status_at(client, &repo, "", &op.id).await.unwrap();
    }
    let new = u64::try_from(back.len()).unwrap() - 2;
    assert_eq!(reads.at_loads() - loads, new);
    daemon.stop().await;
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn effects_are_the_step_delta() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "a.txt", "a\n");
    let w = snapshot(client, &repo, &w).await;
    let v = create(client, &repo, "v@test", None).await;
    write(&v, "v.txt", "v\n");
    snapshot(client, &repo, &v).await;
    client
        .describe_change(v1::DescribeChangeRequest {
            repo: repo.id.clone(),
            revision: w.change_id.clone(),
            description: "described".to_owned(),
            ..v1::DescribeChangeRequest::default()
        })
        .await
        .unwrap();
    client
        .release_workspace(&repo.id, &v.id, true, false)
        .await
        .unwrap();
    let u = create(client, &repo, "u@test", None).await;
    write(&u, "u.txt", "u\n");
    snapshot(client, &repo, &u).await;

    let all = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            include_effects: true,
            page_size: 1000,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations;
    let visible = async |at: &str| -> BTreeSet<String> {
        client
            .list_changes(ListChangesRequest {
                repo: repo.id.clone(),
                revset: "all()".to_owned(),
                at_operation: at.to_owned(),
                ..ListChangesRequest::default()
            })
            .await
            .unwrap()
            .into_iter()
            .map(|entry| entry.change.unwrap().change_id)
            .collect()
    };
    let mut checked = 0;
    for pair in all.windows(2) {
        let (op, parent) = (&pair[0], &pair[1]);
        if op.parent_ids != [parent.id.clone()] || parent.parent_ids.is_empty() {
            continue;
        }
        let effects = op.effects.clone().unwrap();
        // Workspaces: added and forgotten working copies.
        let mut expected = ids(&workspaces_at(client, &repo, &parent.id).await);
        for copy in &effects.working_copies {
            if copy.workspace_id.is_empty() {
                continue;
            }
            if copy.commit_id.is_empty() {
                expected.remove(&copy.workspace_id);
            } else {
                expected.insert(copy.workspace_id.clone());
            }
        }
        assert_eq!(
            ids(&workspaces_at(client, &repo, &op.id).await),
            expected,
            "{}",
            op.description
        );
        // Visible changes: created and abandoned ones.
        assert!(effects.change_count as usize <= effects.changes.len());
        let mut expected = visible(&parent.id).await;
        for change in &effects.changes {
            match change.kind() {
                v1::ChangeEffectKind::Created => {
                    expected.insert(change.change_id.clone());
                }
                v1::ChangeEffectKind::Abandoned => {
                    expected.remove(&change.change_id);
                }
                _ => {}
            }
        }
        assert_eq!(visible(&op.id).await, expected, "{}", op.description);
        checked += 1;
    }
    assert!(checked >= 8, "{checked}");
    daemon.stop().await;
}
