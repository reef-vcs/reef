//! The write side of `reef.api.v1` (R2.03, tests 8 to 16): describe,
//! abandon (resolving a divergent change) and undo through the daemon,
//! with the live workspaces they move, the `reef.*` attributes every
//! operation carries, and the `reef-agent` request metadata.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, AbandonChangeRequest, Attention, CreateWorkspaceRequest, DescribeChangeRequest,
    GetChangeRequest, GetStatusRequest, ListOperationsRequest, MoveWorkspaceRequest,
    OperationFilter, OperationKind, SnapshotKind, UndoOperationRequest, create_workspace_request,
    move_workspace_request,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::testing::{SyntheticRepo, TestDaemon};
use reef_testing::synth::Preset;
use tonic::Code;

/// A daemon whose watcher never snapshots on its own, over a small
/// synthetic repository.
async fn fixture() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
    })
    .await
}

async fn create(client: &Client, repo: &SyntheticRepo, owner: &str) -> v1::Workspace {
    create_at(client, repo, owner, None).await
}

async fn create_at(
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

/// `path` in `commit`'s tree, read from the repository at `root`.
async fn file_at(root: &Path, commit: &str, path: &str) -> Option<String> {
    let repo = reef_lib::jj::Repo::open(root, &reef_daemon::testing::test_settings())
        .await
        .unwrap();
    let path = reef_lib::jj::RepoPath::from_internal_string(path).unwrap();
    repo.read_file_at(&commit.parse().unwrap(), path)
        .await
        .unwrap()
        .map(|bytes| String::from_utf8(bytes).unwrap())
}

fn read(workspace: &v1::Workspace, path: &str) -> Option<String> {
    std::fs::read_to_string(Path::new(&workspace.path).join(path)).ok()
}

fn reason(error: &ClientError) -> (Code, String) {
    match error {
        ClientError::Rpc { status, reason, .. } => (status.code(), reason.clone()),
        other => panic!("{other:?}"),
    }
}

fn metadata(error: &ClientError) -> std::collections::BTreeMap<String, String> {
    match error {
        ClientError::Rpc { metadata, .. } => metadata.clone(),
        other => panic!("{other:?}"),
    }
}

async fn get(client: &Client, repo: &SyntheticRepo, ws: &v1::Workspace) -> v1::Workspace {
    client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn snapshot(client: &Client, repo: &SyntheticRepo, ws: &v1::Workspace) -> bool {
    client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap()
        .changed
}

fn describe(
    ws: &v1::Workspace,
    revision: &str,
    text: &str,
    expected: &str,
) -> DescribeChangeRequest {
    DescribeChangeRequest {
        workspace: ws.id.clone(),
        revision: revision.to_owned(),
        description: text.to_owned(),
        expected_commit_id: expected.to_owned(),
        ..DescribeChangeRequest::default()
    }
}

fn abandon(ws: &v1::Workspace, revision: &str, expected: &str) -> AbandonChangeRequest {
    AbandonChangeRequest {
        workspace: ws.id.clone(),
        revision: revision.to_owned(),
        expected_commit_id: expected.to_owned(),
        ..AbandonChangeRequest::default()
    }
}

fn undo(repo: &SyntheticRepo, ws: Option<&v1::Workspace>, op: &str) -> UndoOperationRequest {
    UndoOperationRequest {
        repo: repo.id.clone(),
        workspace: ws.map(|ws| ws.id.clone()).unwrap_or_default(),
        operation_id: op.to_owned(),
    }
}

async fn change(client: &Client, ws: &v1::Workspace, revision: &str) -> v1::Change {
    client
        .get_change(GetChangeRequest {
            workspace: ws.id.clone(),
            revision: revision.to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

async fn operations(
    client: &Client,
    repo: &SyntheticRepo,
    filter: OperationFilter,
) -> Vec<v1::Operation> {
    client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            filter: Some(filter),
            page_size: 1000,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations
}

async fn divergent_commits(client: &Client, ws: &v1::Workspace) -> usize {
    client
        .list_changes(v1::ListChangesRequest {
            workspace: ws.id.clone(),
            revset: "divergent()".to_owned(),
            skip_snapshot: true,
            ..v1::ListChangesRequest::default()
        })
        .await
        .unwrap()
        .len()
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn describe_change() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo, "owner@test").await;
    let response = client
        .describe_change(describe(&ws, "@", "first line", ""))
        .await
        .unwrap();
    let described = response.change.unwrap();
    assert_eq!(described.description, "first line\n");
    assert_eq!(described.change_id, ws.change_id);
    assert_ne!(described.commit_id, ws.commit_id);
    let operation = response.operation.unwrap();
    assert_eq!(operation.kind(), OperationKind::Describe);
    assert_eq!(operation.workspace_id, ws.id);
    assert_eq!(operation.agent, "owner@test");
    assert_eq!(operation.attributes["reef.kind"], "describe");
    assert!(operation.description.starts_with("reef: describe commit "));
    let effects = operation.effects.unwrap();
    assert!(
        effects
            .changes
            .iter()
            .any(|effect| effect.kind() == v1::ChangeEffectKind::Rewritten),
        "{effects:?}"
    );
    assert_eq!(get(client, &repo, &ws).await.commit_id, described.commit_id);

    // An edit afterwards rewrites the described commit, without divergence.
    write(&ws, "after.txt", "after\n");
    assert!(snapshot(client, &repo, &ws).await);
    let row = get(client, &repo, &ws).await;
    assert!(!row.divergent);
    assert_eq!(change(client, &ws, "@").await.description, "first line\n");
    assert_eq!(divergent_commits(client, &ws).await, 0);

    // Errors.
    let stale = client
        .describe_change(describe(&ws, "@", "x", &described.commit_id))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&stale),
        (
            Code::FailedPrecondition,
            "EXPECTED_COMMIT_MISMATCH".to_owned()
        )
    );
    let meta = metadata(&stale);
    assert_eq!(meta["expected_commit_id"], described.commit_id);
    assert_eq!(meta["commit_id"], row.commit_id);
    let short = client
        .describe_change(describe(&ws, "@", "x", "abc"))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&short),
        (Code::InvalidArgument, "INVALID_COMMIT_ID".to_owned())
    );
    let root = client
        .describe_change(describe(&ws, "root()", "x", ""))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&root),
        (Code::FailedPrecondition, "IMMUTABLE_REVISION".to_owned())
    );
    let empty = client
        .describe_change(describe(&ws, "", "x", ""))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&empty),
        (Code::InvalidArgument, "REVISION_REQUIRED".to_owned())
    );
    let nul = client
        .describe_change(describe(&ws, "@", "a\0b", ""))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&nul),
        (Code::InvalidArgument, "INVALID_DESCRIPTION".to_owned())
    );
    let same = client
        .describe_change(describe(&ws, "@", "first line", &row.commit_id))
        .await
        .unwrap();
    assert!(same.operation.is_none());
    assert_eq!(same.change.unwrap().commit_id, row.commit_id);

    // Median end to end, for the PR.
    let mut times = Vec::new();
    for n in 0..20 {
        let begun = Instant::now();
        client
            .describe_change(describe(&ws, "@", &format!("timed {n}"), ""))
            .await
            .unwrap();
        times.push(begun.elapsed());
    }
    times.sort();
    eprintln!("DescribeChange median over 20: {:?}", times[10]);
    daemon.stop().await;
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn describe_holds_snapshots_off() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        // The watcher snapshots constantly, racing every describe.
        config.file.watcher.debounce = Duration::from_millis(1);
    })
    .await;
    let client = &daemon.client;
    let ws = create(client, &repo, "racer@test").await;
    let writer = {
        let ws = ws.clone();
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(2);
            let mut n = 0u32;
            while Instant::now() < until {
                write(&ws, "racing.txt", &format!("{n}\n"));
                n += 1;
                std::thread::sleep(Duration::from_millis(5));
            }
            n - 1
        })
    };
    for n in 0..40 {
        client
            .describe_change(describe(&ws, "@", &format!("describe {n}"), ""))
            .await
            .unwrap();
    }
    let last = writer.join().unwrap();
    snapshot(client, &repo, &ws).await;
    assert_eq!(divergent_commits(client, &ws).await, 0);
    let row = get(client, &repo, &ws).await;
    assert!(!row.divergent);
    let at = change(client, &ws, "@").await;
    assert_eq!(at.description, "describe 39\n");
    assert_eq!(
        file_at(&repo.root, &at.commit_id, "racing.txt").await,
        Some(format!("{last}\n"))
    );
    daemon.stop().await;
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn abandon_change() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test").await;
    write(&w, "base.txt", "base\n");
    assert!(snapshot(client, &repo, &w).await);
    let w_row = get(client, &repo, &w).await;
    // V works on a child of W's change.
    let v = create_at(
        client,
        &repo,
        "v@test",
        Some(create_workspace_request::Start::NewOn(
            w_row.commit_id.clone(),
        )),
    )
    .await;
    write(&v, "child.txt", "child\n");
    assert!(snapshot(client, &repo, &v).await);
    // Unsnapshotted edits in both.
    write(&w, "late.txt", "late\n");
    write(&v, "child-late.txt", "child late\n");

    // With an edit after the caller's read, the expected id is stale: W's
    // snapshot is recorded and nothing is abandoned.
    let stale = client
        .abandon_change(abandon(&w, "@", &w_row.commit_id))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&stale),
        (
            Code::FailedPrecondition,
            "EXPECTED_COMMIT_MISMATCH".to_owned()
        )
    );
    let w_snapshotted = get(client, &repo, &w).await;
    assert_ne!(w_snapshotted.commit_id, w_row.commit_id, "the snapshot ran");

    let response = client.abandon_change(abandon(&w, "@", "")).await.unwrap();
    assert_eq!(response.abandoned_commit_id, w_snapshotted.commit_id);
    let operation = response.operation.unwrap();
    assert_eq!(operation.kind(), OperationKind::Abandon);
    let ops = operations(
        client,
        &repo,
        OperationFilter {
            workspaces: vec![w.id.clone()],
            ..OperationFilter::default()
        },
    )
    .await;
    assert_eq!(ops[0].id, operation.id);
    assert_eq!(ops[1].kind(), OperationKind::Snapshot);
    assert_eq!(ops[1].snapshot_kind(), SnapshotKind::Explicit);
    // W's files are its parent's tree; it is on a new change.
    assert_eq!(read(&w, "base.txt"), None);
    assert_eq!(read(&w, "late.txt"), None);
    let w_after = get(client, &repo, &w).await;
    assert_ne!(w_after.change_id, w.change_id);
    // V's snapshot before the write records V's owner, not W's.
    let v_snapshot = &operations(
        client,
        &repo,
        OperationFilter {
            workspaces: vec![v.id.clone()],
            kinds: vec![OperationKind::Snapshot.into()],
            ..OperationFilter::default()
        },
    )
    .await[0];
    assert_eq!(v_snapshot.agent, "v@test");
    // V was snapshotted, then rebased: it loses the abandoned diff and
    // keeps its own edits.
    assert_eq!(read(&v, "base.txt"), None);
    assert_eq!(read(&v, "child.txt").as_deref(), Some("child\n"));
    assert_eq!(read(&v, "child-late.txt").as_deref(), Some("child late\n"));
    let v_after = get(client, &repo, &v).await;
    assert_eq!(v_after.change_id, v.change_id);
    assert!(!v_after.divergent);
    let v_change = change(client, &v, "@").await;
    assert!(
        !v_change
            .parent_change_ids
            .contains(&w_snapshotted.change_id),
        "{v_change:?}"
    );
    daemon.stop().await;
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn abandon_resolves_divergence() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let a = create(client, &repo, "a@test").await;
    let b = create_at(
        client,
        &repo,
        "b@test",
        Some(create_workspace_request::Start::Change(a.change_id.clone())),
    )
    .await;
    write(&a, "from-a.txt", "a\n");
    write(&b, "from-b.txt", "b\n");
    assert!(snapshot(client, &repo, &a).await);
    assert!(snapshot(client, &repo, &b).await);
    let (row_a, row_b) = (get(client, &repo, &a).await, get(client, &repo, &b).await);
    assert!(row_b.divergent);

    let ambiguous = client
        .abandon_change(abandon(&a, &a.change_id, ""))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&ambiguous),
        (Code::InvalidArgument, "AMBIGUOUS_REVISION".to_owned())
    );
    let listed = metadata(&ambiguous)["commit_ids"].clone();
    assert!(listed.contains(&row_a.commit_id) && listed.contains(&row_b.commit_id));

    // An edit in B: the write snapshots B first, which rewrites the commit
    // the caller named, so the expected id no longer holds; the error names
    // B's new commit, and a retry with it succeeds.
    write(&b, "late-b.txt", "late\n");
    let moved_on = client
        .abandon_change(abandon(&a, &row_b.commit_id, &row_b.commit_id))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&moved_on),
        (
            Code::FailedPrecondition,
            "EXPECTED_COMMIT_MISMATCH".to_owned()
        )
    );
    let row_b = get(client, &repo, &b).await;
    assert_eq!(metadata(&moved_on)["commit_id"], row_b.commit_id);
    let response = client
        .abandon_change(abandon(&a, &row_b.commit_id, &row_b.commit_id))
        .await
        .unwrap();
    let operation = response.operation.unwrap();
    assert_eq!(operation.kind(), OperationKind::Abandon);
    assert!(
        operation
            .effects
            .unwrap()
            .changes
            .iter()
            .any(|effect| effect.kind() == v1::ChangeEffectKind::Hidden),
    );
    assert_eq!(read(&b, "from-a.txt").as_deref(), Some("a\n"));
    assert_eq!(read(&b, "from-b.txt"), None);
    let (after_a, after_b) = (get(client, &repo, &a).await, get(client, &repo, &b).await);
    assert_eq!(after_b.change_id, a.change_id);
    assert_eq!(after_b.commit_id, row_a.commit_id);
    for row in [&after_a, &after_b] {
        assert!(!row.divergent, "{row:?}");
        assert_eq!(row.attention(), Attention::Unspecified);
    }
    assert_eq!(divergent_commits(client, &a).await, 0);

    // Undo it from B: B's version comes back, divergent again.
    client
        .undo_operation(undo(&repo, Some(&b), &operation.id))
        .await
        .unwrap();
    assert_eq!(read(&b, "from-b.txt").as_deref(), Some("b\n"));
    assert_eq!(read(&b, "from-a.txt"), None);
    let row_b = get(client, &repo, &b).await;
    assert!(row_b.divergent);
    assert_eq!(row_b.attention(), Attention::Divergent);
    assert_eq!(divergent_commits(client, &a).await, 2);
    daemon.stop().await;
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn undo_operation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test").await;
    let described = client
        .describe_change(describe(&w, "@", "named", ""))
        .await
        .unwrap()
        .operation
        .unwrap();
    let response = client
        .undo_operation(undo(&repo, Some(&w), &described.id[..12]))
        .await
        .unwrap();
    assert_eq!(response.undone_operation_id, described.id);
    let undo_op = response.operation.unwrap();
    assert_eq!(undo_op.kind(), OperationKind::Undo);
    assert_eq!(change(client, &w, "@").await.description, "");
    assert_eq!(get(client, &repo, &w).await.commit_id, w.commit_id);
    // Undoing the undo describes it again. Undoing the first undo once
    // more is superseded: what it brought back has been rewritten since.
    client
        .undo_operation(undo(&repo, Some(&w), &undo_op.id))
        .await
        .unwrap();
    assert_eq!(change(client, &w, "@").await.description, "named\n");
    let again = client
        .undo_operation(undo(&repo, Some(&w), &undo_op.id))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&again),
        (Code::FailedPrecondition, "OPERATION_SUPERSEDED".to_owned())
    );

    // A plain abandon, undone from the workspace that abandoned.
    write(&w, "kept.txt", "kept\n");
    assert!(snapshot(client, &repo, &w).await);
    let abandoned = client
        .abandon_change(abandon(&w, "@", ""))
        .await
        .unwrap()
        .operation
        .unwrap();
    assert_eq!(read(&w, "kept.txt"), None);
    client
        .undo_operation(undo(&repo, Some(&w), &abandoned.id))
        .await
        .unwrap();
    assert_eq!(read(&w, "kept.txt").as_deref(), Some("kept\n"));

    // The named workspace's snapshot is reported.
    let other = create(client, &repo, "other@test").await;
    let other_described = client
        .describe_change(describe(&other, "@", "other", ""))
        .await
        .unwrap()
        .operation
        .unwrap();
    write(&w, "edit.txt", "edit\n");
    let response = client
        .undo_operation(undo(&repo, Some(&w), &other_described.id))
        .await
        .unwrap();
    let snapshot = response.snapshot.unwrap();
    assert_eq!(snapshot.kind(), OperationKind::Snapshot);
    assert_eq!(snapshot.workspace_id, w.id);
    assert!(response.operation.is_some());
    daemon.stop().await;
}

// 13
#[tokio::test(flavor = "multi_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one repository walked through every refusal in turn"
)]
async fn undo_refusals() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let created = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "w@test".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap();
    let w = created.workspace.unwrap();
    let not_undoable = |error: ClientError| {
        assert_eq!(
            reason(&error),
            (Code::InvalidArgument, "OPERATION_NOT_UNDOABLE".to_owned())
        );
    };
    not_undoable(
        client
            .undo_operation(undo(&repo, None, &created.operation.unwrap().id))
            .await
            .unwrap_err(),
    );
    let target = create(client, &repo, "target@test").await;
    let moved = client
        .move_workspace(MoveWorkspaceRequest {
            workspace: w.id.clone(),
            target: Some(move_workspace_request::Target::NewOn(
                target.commit_id.clone(),
            )),
            ..MoveWorkspaceRequest::default()
        })
        .await
        .unwrap();
    not_undoable(
        client
            .undo_operation(undo(&repo, None, &moved.operation.unwrap().id))
            .await
            .unwrap_err(),
    );
    let released = client
        .release_workspace(&repo.id, &target.id, false, true)
        .await
        .unwrap();
    not_undoable(
        client
            .undo_operation(undo(&repo, None, &released.operation.unwrap().id))
            .await
            .unwrap_err(),
    );
    let at = client
        .undo_operation(undo(&repo, None, "@-"))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&at),
        (Code::InvalidArgument, "INVALID_OPERATION_ID".to_owned())
    );
    let unknown = client
        .undo_operation(undo(&repo, None, &"ab".repeat(64)))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&unknown),
        (Code::NotFound, "OPERATION_NOT_FOUND".to_owned())
    );

    // A describe, an edit, then the describe undone: the edit's snapshot
    // supersedes it.
    let described = client
        .describe_change(describe(&w, "@", "named", ""))
        .await
        .unwrap()
        .operation
        .unwrap();
    write(&w, "edit.txt", "edit\n");
    let superseded = client
        .undo_operation(undo(&repo, Some(&w), &described.id))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&superseded),
        (Code::FailedPrecondition, "OPERATION_SUPERSEDED".to_owned())
    );
    let at = change(client, &w, "@").await;
    assert_eq!(at.description, "named\n");
    assert_eq!(
        file_at(&repo.root, &at.commit_id, "edit.txt")
            .await
            .as_deref(),
        Some("edit\n"),
        "the edit was snapshotted into W's commit"
    );
    assert_eq!(read(&w, "edit.txt").as_deref(), Some("edit\n"));

    // A released workspace cannot be named.
    let gone = client
        .undo_operation(undo(&repo, Some(&target), &described.id))
        .await
        .unwrap_err();
    assert_eq!(reason(&gone).0, Code::FailedPrecondition);
    daemon.stop().await;
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn undo_moves_other_workspaces() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let a = create(client, &repo, "a@test").await;
    let b = create(client, &repo, "b@test").await;
    write(&b, "b.txt", "b\n");
    let snapshot_op = client
        .snapshot_workspace(&repo.id, &b.id, "")
        .await
        .unwrap()
        .operation
        .unwrap();
    let before = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            page_size: 1000,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations
        .len();
    client
        .undo_operation(undo(&repo, Some(&a), &snapshot_op.id))
        .await
        .unwrap();
    assert_eq!(read(&b, "b.txt"), None);
    assert_eq!(get(client, &repo, &b).await.commit_id, b.commit_id);
    let after = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            page_size: 1000,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations
        .len();
    assert_eq!(after, before + 1, "only the undo was recorded");

    // With an unsnapshotted edit in B, B is snapshotted first, and the
    // undo is superseded.
    write(&b, "b.txt", "again\n");
    let resnapshot = client
        .snapshot_workspace(&repo.id, &b.id, "")
        .await
        .unwrap()
        .operation
        .unwrap();
    write(&b, "more.txt", "more\n");
    let error = client
        .undo_operation(undo(&repo, Some(&a), &resnapshot.id))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "OPERATION_SUPERSEDED".to_owned())
    );
    assert_eq!(read(&b, "more.txt").as_deref(), Some("more\n"));
    daemon.stop().await;
}

// 15 (the RPC half: each write's own operation, listed, equals what the
// RPC returned, and carries the attributes)
#[tokio::test(flavor = "multi_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "one session of every write, then each checked"
)]
async fn every_operation_is_tagged() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let created = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "o@test".to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap();
    let ws = created.workspace.unwrap();
    write(&ws, "a.txt", "a\n");
    let snapshot = client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap()
        .operation
        .unwrap();
    write(&ws, "b.txt", "b\n");
    let read_snapshot = client
        .get_status(GetStatusRequest {
            workspace: ws.id.clone(),
            ..GetStatusRequest::default()
        })
        .await
        .unwrap()
        .workspace_status
        .unwrap()
        .snapshot
        .unwrap();
    let described = client
        .describe_change(describe(&ws, "@", "d", ""))
        .await
        .unwrap()
        .operation
        .unwrap();
    let undone = client
        .undo_operation(undo(&repo, Some(&ws), &described.id))
        .await
        .unwrap()
        .operation
        .unwrap();
    write(&ws, "c.txt", "c\n");
    let inspected = client
        .inspect_workspace(&repo.id, &ws.id, true)
        .await
        .unwrap()
        .snapshot
        .unwrap();
    write(&ws, "d.txt", "d\n");
    let moved = client
        .move_workspace(MoveWorkspaceRequest {
            workspace: ws.id.clone(),
            target: Some(move_workspace_request::Target::NewOn("@-".to_owned())),
            ..MoveWorkspaceRequest::default()
        })
        .await
        .unwrap();
    write(&ws, "e.txt", "e\n");
    let released = client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    let written = [
        (released.final_snapshot.unwrap(), "snapshot", Some("final")),
        (inspected, "snapshot", Some("explicit")),
        (moved.final_snapshot.unwrap(), "snapshot", Some("move")),
        (moved.operation.unwrap(), "move-workspace", None),
        (created.operation.unwrap(), "create-workspace", None),
        (snapshot, "snapshot", Some("explicit")),
        (read_snapshot, "snapshot", Some("explicit")),
        (described, "describe", None),
        (undone, "undo", None),
        (released.operation.unwrap(), "release-workspace", None),
    ];
    for (operation, kind, snapshot_kind) in &written {
        assert_eq!(operation.attributes["reef.kind"], *kind, "{operation:?}");
        assert_eq!(
            operation.attributes["reef.workspace"], ws.id,
            "{operation:?}"
        );
        assert_eq!(
            operation.attributes["reef.agent"], "o@test",
            "{operation:?}"
        );
        assert_eq!(
            operation
                .attributes
                .get("reef.snapshot-kind")
                .map(String::as_str),
            *snapshot_kind
        );
        let listed = client
            .get_operation(&repo.id, &operation.id)
            .await
            .unwrap()
            .operation
            .unwrap();
        assert_eq!(listed.kind, operation.kind, "{kind}");
        assert_eq!(listed.snapshot_kind, operation.snapshot_kind, "{kind}");
        assert_eq!(listed.workspace_id, operation.workspace_id, "{kind}");
        assert_eq!(listed.agent, operation.agent, "{kind}");
        assert_eq!(listed.effects, operation.effects, "{kind}");
    }
    daemon.stop().await;
}

// 16
#[tokio::test(flavor = "multi_thread")]
async fn agent_metadata() {
    let (daemon, repo) = fixture().await;
    let planner = daemon.client.clone().with_agent("planner-1").unwrap();
    let ws = create(&planner, &repo, "owner@test").await;
    write(&ws, "a.txt", "a\n");
    let snapshot = planner
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap()
        .operation
        .unwrap();
    assert_eq!(snapshot.agent, "planner-1");
    write(&ws, "b.txt", "b\n");
    planner
        .get_change(GetChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".to_owned(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap();
    let described = planner
        .describe_change(describe(&ws, "@", "planned", ""))
        .await
        .unwrap()
        .operation
        .unwrap();
    assert_eq!(described.agent, "planner-1");
    // Without the entry, the owner.
    let owned = daemon
        .client
        .describe_change(describe(&ws, "@", "owned", ""))
        .await
        .unwrap()
        .operation
        .unwrap();
    assert_eq!(owned.agent, "owner@test");

    let by_planner = operations(
        &daemon.client,
        &repo,
        OperationFilter {
            agents: vec!["planner-1".to_owned()],
            ..OperationFilter::default()
        },
    )
    .await;
    let kinds: BTreeSet<i32> = by_planner.iter().map(|op| op.kind).collect();
    assert_eq!(
        by_planner.len(),
        4,
        "create, two snapshots, describe: {by_planner:?}"
    );
    assert!(kinds.contains(&(OperationKind::CreateWorkspace as i32)));
    assert!(by_planner.iter().all(|op| op.agent == "planner-1"));

    assert!(matches!(
        daemon.client.clone().with_agent("x".repeat(257)),
        Err(ClientError::InvalidAgent(_))
    ));
    // A raw request with a long value is refused and writes nothing.
    let channel = daemon.client.clone();
    let before = operations(&channel, &repo, OperationFilter::default())
        .await
        .len();
    let mut raw = v1::change_service_client::ChangeServiceClient::new(
        raw_channel(daemon.handle.socket_path()).await,
    );
    let mut request = tonic::Request::new(describe(&ws, "@", "too long", ""));
    request
        .metadata_mut()
        .insert("reef-agent", "x".repeat(257).parse().unwrap());
    let error = raw.describe_change(request).await.unwrap_err();
    assert_eq!(error.code(), Code::InvalidArgument);
    assert_eq!(
        reef_api::error::detail(&error).unwrap().reason,
        "INVALID_AGENT"
    );
    assert_eq!(
        operations(&channel, &repo, OperationFilter::default())
            .await
            .len(),
        before
    );
    daemon.stop().await;
}

/// A plain channel to the daemon, for requests the client would not send.
async fn raw_channel(socket: &Path) -> tonic::transport::Channel {
    let path = socket.to_owned();
    tonic::transport::Endpoint::from_static("http://[::]:0")
        .connect_with_connector(tower::service_fn(move |_: tonic::transport::Uri| {
            let path = path.clone();
            async move {
                Ok::<_, std::io::Error>(hyper_util::rt::TokioIo::new(
                    tokio::net::UnixStream::connect(path).await?,
                ))
            }
        }))
        .await
        .unwrap()
}
