//! The read side of `reef.api.v1` (R2.02, tests 14 to 24): status, the
//! log, changes, diffs and the operation log, read through the daemon, and
//! revisions as revsets for creates and moves.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeSet;
use std::path::Path;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, CreateWorkspaceRequest, DiffFormat, GetChangeRequest, GetDiffRequest, GetStatusRequest,
    ListChangesRequest, ListOperationsRequest, ListWorkspacesRequest, MoveWorkspaceRequest,
    OperationFilter, OperationKind, SnapshotKind, StatusSource, create_workspace_request,
    move_workspace_request,
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

/// Runs `f` on a jj transaction of the repository at `root`, from outside
/// the daemon as a user's `jj` would, and commits it.
fn with_tx(root: &Path, description: &str, f: impl FnOnce(&mut jj_lib::transaction::Transaction)) {
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
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
    f(&mut tx);
    pollster::block_on(tx.repo_mut().rebase_descendants()).unwrap();
    pollster::block_on(tx.commit(description)).unwrap();
}

fn set_bookmark(root: &Path, name: &str, commit: &str) {
    use jj_lib::backend::CommitId;
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;
    with_tx(root, "bookmark set", |tx| {
        tx.repo_mut().set_local_bookmark_target(
            RefName::new(name),
            RefTarget::normal(CommitId::try_from_hex(commit).unwrap()),
        );
    });
}

async fn create(client: &Client, repo: &SyntheticRepo, owner: &str) -> v1::Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: owner.to_owned(),
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

fn reason(error: &ClientError) -> (Code, String) {
    match error {
        ClientError::Rpc { status, reason, .. } => (status.code(), reason.clone()),
        other => panic!("{other:?}"),
    }
}

fn status_request(workspace: &v1::Workspace) -> GetStatusRequest {
    GetStatusRequest {
        workspace: workspace.id.clone(),
        ..GetStatusRequest::default()
    }
}

/// Snapshot operations recorded for `workspace`, newest first.
async fn snapshots_of(
    client: &Client,
    repo: &SyntheticRepo,
    workspace: &str,
) -> Vec<v1::Operation> {
    client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            filter: Some(OperationFilter {
                workspaces: vec![workspace.to_owned()],
                kinds: vec![OperationKind::Snapshot.into()],
                ..OperationFilter::default()
            }),
            page_size: 1000,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn status_snapshots_first() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo, "status@test").await;
    let other = create(client, &repo, "other@test").await;
    write(&ws, "status-edit.txt", "one\n");

    let status = client.get_status(status_request(&ws)).await.unwrap();
    let found = status.workspace_status.as_ref().unwrap();
    // The index (R2.23); on Linux a walk until the create's background
    // attach is up.
    assert!(
        matches!(found.source(), StatusSource::Index | StatusSource::Walk),
        "{found:?}"
    );
    let snapshot = found.snapshot.as_ref().expect("a snapshot was recorded");
    assert_eq!(snapshot.kind(), OperationKind::Snapshot);
    assert_eq!(snapshot.snapshot_kind(), SnapshotKind::Explicit);
    assert!(snapshot.effects.is_some());
    let paths: Vec<&str> = found
        .changed_files
        .iter()
        .map(|f| f.path.as_str())
        .collect();
    assert_eq!(paths, ["status-edit.txt"]);
    assert_eq!(found.changed_file_count, 1);
    assert_eq!(
        found.working_copy.as_ref().unwrap().workspace_ids,
        std::slice::from_ref(&ws.id)
    );
    assert_eq!(found.parents.len(), 1);
    let repository = status.repository_status.as_ref().unwrap();
    assert_eq!(repository.trunk_bookmark, "main");
    assert!(repository.trunk.as_ref().unwrap().immutable);
    assert_eq!(repository.active_workspace_count, 2);
    let listed: BTreeSet<&str> = repository
        .active_workspaces
        .iter()
        .map(|w| w.id.as_str())
        .collect();
    assert_eq!(listed, BTreeSet::from([ws.id.as_str(), other.id.as_str()]));
    assert!(repository.head_count >= 2, "{}", repository.head_count);
    let last = repository.last_operation.as_ref().unwrap();
    assert_eq!(last.id, status.at_operation);
    assert!(last.effects.is_some());

    // A second edit, with skip_snapshot: the last snapshot, nothing written.
    write(&ws, "second.txt", "two\n");
    let before = snapshots_of(client, &repo, &ws.id).await.len();
    let skipped = client
        .get_status(GetStatusRequest {
            skip_snapshot: true,
            ..status_request(&ws)
        })
        .await
        .unwrap();
    let found = skipped.workspace_status.unwrap();
    assert_eq!(found.source(), StatusSource::LastSnapshot);
    assert!(found.snapshot.is_none());
    assert_eq!(found.changed_file_count, 1, "second.txt is not snapshotted");
    assert_eq!(snapshots_of(client, &repo, &ws.id).await.len(), before);

    // max_files caps the list, never the count.
    for n in 0..150 {
        write(&ws, &format!("many/{n:03}.txt"), "x\n");
    }
    let capped = client.get_status(status_request(&ws)).await.unwrap();
    let found = capped.workspace_status.unwrap();
    assert_eq!(
        (found.changed_files.len(), found.changed_file_count),
        (100, 152)
    );
    let all = client
        .get_status(GetStatusRequest {
            max_files: 5000,
            ..status_request(&ws)
        })
        .await
        .unwrap();
    assert_eq!(all.workspace_status.unwrap().changed_files.len(), 152);
    let error = client
        .get_status(GetStatusRequest {
            max_files: -1,
            ..status_request(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).0, Code::InvalidArgument);

    // The repository alone.
    let alone = client
        .get_status(GetStatusRequest {
            repo: repo.id.clone(),
            ..GetStatusRequest::default()
        })
        .await
        .unwrap();
    assert!(alone.workspace_status.is_none());

    client
        .release_workspace("", &other.id, true, false)
        .await
        .unwrap();
    let error = client.get_status(status_request(&other)).await.unwrap_err();
    assert_eq!(reason(&error).0, Code::FailedPrecondition);
    daemon.stop().await;
}

// 15
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "a divergent change, then a conflicted merge"
)]
async fn status_reports_conflicts_and_divergence() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let first = create(client, &repo, "a@test").await;
    // A second workspace on the same change; both edit it.
    let second = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "b@test".into(),
            start: Some(create_workspace_request::Start::Change(
                first.commit_id.clone(),
            )),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    write(&first, "from-a.txt", "a\n");
    write(&second, "from-b.txt", "b\n");
    for ws in [&first, &second] {
        client.snapshot_workspace("", &ws.id, "").await.unwrap();
    }
    let status = client
        .get_status(GetStatusRequest {
            repo: repo.id.clone(),
            ..GetStatusRequest::default()
        })
        .await
        .unwrap();
    let repository = status.repository_status.unwrap();
    assert!(
        repository.divergent_change_ids.contains(&first.change_id),
        "{:?}",
        repository.divergent_change_ids
    );

    // A conflicted merge, made outside the daemon, edited by a workspace.
    let main = Repo::open(&repo.root, &test_settings())
        .await
        .unwrap()
        .wc_change()
        .unwrap()
        .commit_id;
    let mut merge_id = String::new();
    with_tx(&repo.root, "conflict", |tx| {
        use jj_lib::backend::{CopyId, TreeValue};
        use jj_lib::merge::Merge;
        use jj_lib::object_id::ObjectId as _;
        use jj_lib::repo::Repo as _;
        use jj_lib::repo_path::RepoPathBuf;
        let store = tx.repo().store().clone();
        let base = store
            .get_commit(&jj_lib::backend::CommitId::try_from_hex(main.as_str()).unwrap())
            .unwrap();
        let mut sides = Vec::new();
        for text in ["x\n", "y\n"] {
            let path = RepoPathBuf::from_internal_string("conflicted.txt").unwrap();
            let id = pollster::block_on(store.write_file(&path, &mut text.as_bytes())).unwrap();
            let mut builder = jj_lib::merged_tree_builder::MergedTreeBuilder::new(base.tree());
            builder.set_or_remove(
                path,
                Merge::normal(TreeValue::File {
                    id,
                    executable: false,
                    copy_id: CopyId::placeholder(),
                }),
            );
            let tree = pollster::block_on(builder.write_tree()).unwrap();
            sides.push(
                pollster::block_on(
                    tx.repo_mut()
                        .new_commit(vec![base.id().clone()], tree)
                        .set_description(text.trim())
                        .write(),
                )
                .unwrap(),
            );
        }
        let merged =
            pollster::block_on(jj_lib::rewrite::merge_commit_trees(tx.repo(), &sides)).unwrap();
        let merge = pollster::block_on(
            tx.repo_mut()
                .new_commit(sides.iter().map(|c| c.id().clone()).collect(), merged)
                .set_description("merge")
                .write(),
        )
        .unwrap();
        merge_id = merge.id().hex();
    });
    let conflicted = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "c@test".into(),
            start: Some(create_workspace_request::Start::Change(merge_id.clone())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    let status = client
        .get_status(status_request(&conflicted))
        .await
        .unwrap();
    let found = status.workspace_status.unwrap();
    assert_eq!(found.conflicted_paths, ["conflicted.txt"]);
    assert_eq!(found.conflicted_path_count, 1);
    assert!(found.working_copy.unwrap().conflict);
    let repository = status.repository_status.unwrap();
    assert!(
        repository
            .conflicted_change_ids
            .contains(&conflicted.change_id),
        "{:?}",
        repository.conflicted_change_ids
    );
    daemon.stop().await;
}

// 16
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_reads_share_a_snapshot() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo, "concurrent@test").await;
    let before = snapshots_of(client, &repo, &ws.id).await.len();
    write(&ws, "shared.txt", "seen by all\n");
    let reads = (0..8).map(|_| {
        let client = client.clone();
        let request = status_request(&ws);
        tokio::spawn(async move { client.get_status(request).await.unwrap() })
    });
    for read in futures::future::join_all(reads).await {
        let status = read.unwrap().workspace_status.unwrap();
        assert!(
            status.changed_files.iter().any(|f| f.path == "shared.txt"),
            "every read sees the edit"
        );
    }
    let after = snapshots_of(client, &repo, &ws.id).await.len();
    assert!(after - before <= 2, "{} snapshots", after - before);
    assert!(after > before);
    daemon.stop().await;
}

// 17
#[tokio::test(flavor = "multi_thread")]
async fn list_changes() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo, "log@test").await;
    let log = client
        .list_changes(ListChangesRequest {
            repo: repo.id.clone(),
            ..ListChangesRequest::default()
        })
        .await
        .unwrap();
    let commits: BTreeSet<&str> = log
        .iter()
        .map(|entry| entry.change.as_ref().unwrap().commit_id.as_str())
        .collect();
    assert!(
        commits.contains(ws.commit_id.as_str()),
        "the workspace's working copy"
    );
    let trunk = log
        .iter()
        .find(|entry| {
            entry
                .change
                .as_ref()
                .unwrap()
                .bookmarks
                .contains(&"main".to_owned())
        })
        .expect("trunk is in the default set");
    assert!(trunk.change.as_ref().unwrap().immutable);

    // `@-` with a workspace snapshots first.
    write(&ws, "log-edit.txt", "x\n");
    let before = snapshots_of(client, &repo, &ws.id).await.len();
    let parents = client
        .list_changes(ListChangesRequest {
            workspace: ws.id.clone(),
            revset: "@-".into(),
            ..ListChangesRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(parents.len(), 1);
    assert_eq!(snapshots_of(client, &repo, &ws.id).await.len(), before + 1);
    // Another workspace's working copy, and every one, never snapshot.
    write(&ws, "log-edit-2.txt", "y\n");
    for revset in [format!("reef-{}@", ws.id), "working_copies()".to_owned()] {
        client
            .list_changes(ListChangesRequest {
                repo: repo.id.clone(),
                revset,
                ..ListChangesRequest::default()
            })
            .await
            .unwrap();
    }
    assert_eq!(snapshots_of(client, &repo, &ws.id).await.len(), before + 1);

    for (revset, expected) in [
        (
            "@-",
            (Code::InvalidArgument, reef_api::error::WORKSPACE_REQUIRED),
        ),
        (
            "main |",
            (Code::InvalidArgument, reef_api::error::INVALID_REVSET),
        ),
        (
            "nope",
            (Code::NotFound, reef_api::error::REVISION_NOT_FOUND),
        ),
    ] {
        let error = client
            .list_changes(ListChangesRequest {
                repo: repo.id.clone(),
                revset: revset.into(),
                ..ListChangesRequest::default()
            })
            .await
            .unwrap_err();
        assert_eq!(
            reason(&error),
            (expected.0, expected.1.to_owned()),
            "{revset}"
        );
    }
    daemon.stop().await;
}

// 18
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "the three paged reads, each in turn")]
async fn pages_are_pinned() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let mut workspaces = Vec::new();
    for n in 0..5 {
        workspaces.push(create(client, &repo, &format!("page{n}@test")).await);
    }
    let request = ListChangesRequest {
        repo: repo.id.clone(),
        revset: "mutable()".into(),
        page_size: 2,
        ..ListChangesRequest::default()
    };
    let first = client.list_changes_page(request.clone()).await.unwrap();
    assert_eq!(first.entries.len(), 2);
    // A workspace created between pages is not on later pages.
    let late = create(client, &repo, "late@test").await;
    let mut seen: Vec<String> = first
        .entries
        .iter()
        .map(|e| e.change.as_ref().unwrap().commit_id.clone())
        .collect();
    let mut token = first.next_page_token.clone();
    while !token.is_empty() {
        let page = client
            .list_changes_page(ListChangesRequest {
                page_token: token,
                ..request.clone()
            })
            .await
            .unwrap();
        assert_eq!(page.at_operation, first.at_operation, "pinned");
        seen.extend(
            page.entries
                .iter()
                .map(|e| e.change.as_ref().unwrap().commit_id.clone()),
        );
        token = page.next_page_token;
    }
    let unique: BTreeSet<&String> = seen.iter().collect();
    assert_eq!(unique.len(), seen.len(), "no duplicates");
    assert!(!seen.contains(&late.commit_id));
    let whole = client
        .list_changes(ListChangesRequest {
            at_operation: first.at_operation.clone(),
            ..request.clone()
        })
        .await
        .unwrap();
    assert_eq!(whole.len(), seen.len(), "no gaps");
    let error = client
        .list_changes_page(ListChangesRequest {
            page_token: first.next_page_token.clone(),
            revset: "all()".into(),
            ..request.clone()
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (
            Code::InvalidArgument,
            reef_api::error::INVALID_PAGE_TOKEN.to_owned()
        )
    );

    // A diff of 250 files over three pages, none snapshotting.
    let ws = &workspaces[0];
    for n in 0..250 {
        write(ws, &format!("paged/{n:03}.txt"), "p\n");
    }
    let diff_request = GetDiffRequest {
        workspace: ws.id.clone(),
        page_size: 100,
        ..GetDiffRequest::default()
    };
    let page = client.get_diff_page(diff_request.clone()).await.unwrap();
    assert_eq!((page.files.len(), page.total_files), (100, 250));
    let snapshots = snapshots_of(client, &repo, &ws.id).await.len();
    write(ws, "paged/zzz-late.txt", "late\n");
    let mut paths: Vec<String> = page
        .files
        .iter()
        .map(|f| f.change.as_ref().unwrap().path.clone())
        .collect();
    let mut token = page.next_page_token;
    let mut pages = 1;
    while !token.is_empty() {
        let next = client
            .get_diff_page(GetDiffRequest {
                page_token: token,
                ..diff_request.clone()
            })
            .await
            .unwrap();
        assert_eq!(next.total_files, 250);
        assert_eq!(next.at_operation, page.at_operation);
        paths.extend(
            next.files
                .iter()
                .map(|f| f.change.as_ref().unwrap().path.clone()),
        );
        token = next.next_page_token;
        pages += 1;
    }
    assert_eq!(pages, 3);
    assert_eq!(paths.len(), 250);
    assert!(paths.windows(2).all(|pair| pair[0] < pair[1]), "path order");
    assert_eq!(
        snapshots_of(client, &repo, &ws.id).await.len(),
        snapshots,
        "later pages never snapshot"
    );

    // The operation log, pinned to the first page's heads.
    let ops_request = ListOperationsRequest {
        repo: repo.id.clone(),
        page_size: 3,
        ..ListOperationsRequest::default()
    };
    let first = client
        .list_operations_page(ops_request.clone())
        .await
        .unwrap();
    let newest = first.operations[0].id.clone();
    create(client, &repo, "more@test").await;
    let mut ids: Vec<String> = first.operations.iter().map(|op| op.id.clone()).collect();
    let mut token = first.next_page_token;
    while !token.is_empty() {
        let page = client
            .list_operations_page(ListOperationsRequest {
                page_token: token,
                ..ops_request.clone()
            })
            .await
            .unwrap();
        ids.extend(page.operations.iter().map(|op| op.id.clone()));
        token = page.next_page_token;
    }
    assert_eq!(ids[0], newest);
    let unique: BTreeSet<&String> = ids.iter().collect();
    assert_eq!(unique.len(), ids.len());
    let now = client
        .list_operations_page(ListOperationsRequest {
            page_size: 1000,
            ..ops_request
        })
        .await
        .unwrap();
    assert_eq!(
        now.operations.len(),
        ids.len() + 1,
        "the new create is newer than the pinned heads"
    );
    daemon.stop().await;
}

// 19
#[tokio::test(flavor = "multi_thread")]
async fn reads_at_an_operation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo, "at@test").await;
    let before = client
        .get_change(GetChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".into(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap();
    let old = before.change.unwrap();
    write(&ws, "rewrite.txt", "new\n");
    let snapshot = client.snapshot_workspace("", &ws.id, "").await.unwrap();
    assert!(snapshot.changed);
    let operations = snapshots_of(client, &repo, &ws.id).await.len();
    let then = client
        .get_change(GetChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".into(),
            at_operation: before.at_operation.clone(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(then.change.unwrap().commit_id, old.commit_id);
    assert_eq!(then.at_operation, before.at_operation);
    assert_eq!(
        snapshots_of(client, &repo, &ws.id).await.len(),
        operations,
        "never snapshots"
    );
    let hidden = client
        .get_change(GetChangeRequest {
            repo: repo.id.clone(),
            revision: old.commit_id.clone(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert!(hidden.hidden);
    for (at, expected) in [
        (
            "zz",
            (Code::InvalidArgument, reef_api::error::INVALID_OPERATION_ID),
        ),
        (
            "f".repeat(before.at_operation.len()).as_str(),
            (Code::NotFound, reef_api::error::OPERATION_NOT_FOUND),
        ),
    ] {
        let error = client
            .get_change(GetChangeRequest {
                repo: repo.id.clone(),
                revision: "main".into(),
                at_operation: at.to_owned(),
                ..GetChangeRequest::default()
            })
            .await
            .unwrap_err();
        assert_eq!(reason(&error), (expected.0, expected.1.to_owned()), "{at}");
    }
    let error = client
        .get_change(GetChangeRequest {
            repo: repo.id.clone(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, reef_api::error::REVISION_REQUIRED);
    daemon.stop().await;
}

// 20
#[tokio::test(flavor = "multi_thread")]
async fn get_diff() {
    // The long files below are above the default 1 MiB, which a snapshot
    // would leave untracked.
    let (daemon, repo) = fixture_with(|config| {
        config.file.snapshot.max_new_file_size = reef_daemon::config::ByteSize(32 << 20);
    })
    .await;
    let client = &daemon.client;
    let error = client
        .get_diff_page(GetDiffRequest {
            repo: repo.id.clone(),
            ..GetDiffRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, reef_api::error::WORKSPACE_REQUIRED);
    let ws = create(client, &repo, "diff@test").await;
    write(&ws, "small.txt", "a\nb\nc\n");
    let stat = client
        .get_diff_page(GetDiffRequest {
            workspace: ws.id.clone(),
            format: DiffFormat::Stat.into(),
            ..GetDiffRequest::default()
        })
        .await
        .unwrap();
    let small = &stat.files[0];
    assert_eq!((small.lines_added, small.lines_removed), (3, 0));
    assert_eq!(small.hunks, []);
    let hunks = client
        .get_diff_page(GetDiffRequest {
            workspace: ws.id.clone(),
            format: DiffFormat::Hunks.into(),
            ..GetDiffRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(hunks.files[0].hunks[0].lines, "+a\n+b\n+c\n");
    assert_eq!(hunks.from_commit_ids.len(), 1);

    // Long files: cut at max_lines_per_file, or at 1 MiB of hunk lines.
    let long: String = (0..100_000).fold(String::new(), |mut text, n| {
        use std::fmt::Write as _;
        writeln!(text, "line {n:06} of a long file").unwrap();
        text
    });
    for n in 0..10 {
        write(&ws, &format!("long/{n}.txt"), &long);
    }
    let request = GetDiffRequest {
        workspace: ws.id.clone(),
        format: DiffFormat::Hunks.into(),
        paths: vec!["long".into()],
        ..GetDiffRequest::default()
    };
    let page = client.get_diff_page(request.clone()).await.unwrap();
    let file = &page.files[0];
    assert!(file.truncated);
    assert_eq!(file.lines_added, 100_000);
    let kept: usize = file.hunks.iter().map(|h| h.lines.lines().count()).sum();
    assert_eq!(kept, 1000);
    let wide = GetDiffRequest {
        max_lines_per_file: 500_000,
        skip_snapshot: true,
        ..request
    };
    let page = client.get_diff_page(wide.clone()).await.unwrap();
    assert!(page.files.len() <= 3, "{} files", page.files.len());
    assert_ne!(page.next_page_token, "");
    let bytes: usize = page
        .files
        .iter()
        .flat_map(|f| f.hunks.iter().map(|h| h.lines.len()))
        .sum();
    assert!(bytes <= 3 << 20, "{bytes}");
    for file in &page.files {
        assert!(file.truncated);
        let lines: usize = file.hunks.iter().map(|h| h.lines.len()).sum();
        assert!(lines <= 1 << 20, "{lines}");
    }
    let whole = client.get_diff(wide).await.unwrap();
    assert_eq!(whole.files.len(), 10);
    let error = client
        .get_diff_page(GetDiffRequest {
            workspace: ws.id.clone(),
            paths: vec!["../x".into()],
            ..GetDiffRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, reef_api::error::INVALID_PATH);
    daemon.stop().await;
}

// 21
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one workspace's life, then every filter"
)]
async fn operations() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.watcher.debounce = Duration::from_millis(50);
    })
    .await;
    let client = &daemon.client;
    let owner = "ops@test";
    let ws = create(client, &repo, owner).await;
    write(&ws, "watched.txt", "w\n");
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let found = snapshots_of(client, &repo, &ws.id).await;
        if found
            .iter()
            .any(|op| op.snapshot_kind() == SnapshotKind::Watcher)
        {
            break;
        }
        assert!(Instant::now() < deadline, "no watcher snapshot");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // The watcher stays on (debounce 50 ms), so it may snapshot each write
    // before the RPC that follows it does; that RPC then records no
    // snapshot. The expected list below follows what each RPC reports, and
    // a watcher snapshot must stand in for each one that recorded none.
    // `writes::every_operation_is_tagged` checks the explicit, move and
    // final kinds with the watcher off.
    write(&ws, "explicit.txt", "e\n");
    let explicit = client
        .snapshot_workspace("", &ws.id, "")
        .await
        .unwrap()
        .operation
        .is_some();
    write(&ws, "before-move.txt", "m\n");
    let moved = client
        .move_workspace(MoveWorkspaceRequest {
            workspace: ws.id.clone(),
            target: Some(move_workspace_request::Target::NewOn("@".into())),
            ..MoveWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .final_snapshot
        .is_some();
    write(&ws, "before-release.txt", "r\n");
    let released = client
        .release_workspace("", &ws.id, true, false)
        .await
        .unwrap()
        .final_snapshot
        .is_some();
    // An operation the user writes on the root with jj.
    with_tx(&repo.root, "new empty commit", |_| {});

    let all = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            page_size: 1000,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations;
    let external = &all[0];
    assert_eq!(external.kind(), OperationKind::External);
    assert_eq!(external.description, "new empty commit");
    assert_eq!(external.workspace_id, "");
    let ours: Vec<(OperationKind, SnapshotKind)> = all
        .iter()
        .filter(|op| op.workspace_id == ws.id)
        .map(|op| (op.kind(), op.snapshot_kind()))
        .collect();
    let watcher = (OperationKind::Snapshot, SnapshotKind::Watcher);
    assert!(ours.contains(&watcher), "{ours:?}");
    let expected: Vec<(OperationKind, SnapshotKind)> = [
        Some((OperationKind::ReleaseWorkspace, SnapshotKind::Unspecified)),
        released.then_some((OperationKind::Snapshot, SnapshotKind::Final)),
        Some((OperationKind::MoveWorkspace, SnapshotKind::Unspecified)),
        moved.then_some((OperationKind::Snapshot, SnapshotKind::Move)),
        explicit.then_some((OperationKind::Snapshot, SnapshotKind::Explicit)),
        Some((OperationKind::CreateWorkspace, SnapshotKind::Unspecified)),
    ]
    .into_iter()
    .flatten()
    .collect();
    let not_watcher: Vec<(OperationKind, SnapshotKind)> =
        ours.iter().copied().filter(|op| *op != watcher).collect();
    assert_eq!(not_watcher, expected);
    // Newest first: every watcher snapshot is between the create and the
    // release, and each write an RPC did not record was the watcher's.
    let at = |kind: OperationKind| ours.iter().position(|op| op.0 == kind).unwrap();
    let watchers = |from: usize, to: usize| {
        ours[from + 1..to]
            .iter()
            .filter(|op| **op == watcher)
            .count()
    };
    let (release_at, move_at, create_at) = (
        at(OperationKind::ReleaseWorkspace),
        at(OperationKind::MoveWorkspace),
        at(OperationKind::CreateWorkspace),
    );
    assert_eq!(
        watchers(release_at, create_at),
        ours.iter().filter(|op| **op == watcher).count(),
        "{ours:?}"
    );
    assert!(
        watchers(release_at, move_at) >= usize::from(!released),
        "{ours:?}"
    );
    assert!(
        watchers(move_at, create_at) >= 1 + usize::from(!moved) + usize::from(!explicit),
        "{ours:?}"
    );
    assert!(
        all.iter()
            .filter(|op| op.workspace_id == ws.id)
            .all(|op| op.agent == owner)
    );
    assert!(all.iter().all(|op| op.effects.is_none()), "only on request");

    let list = |filter: OperationFilter| {
        let client = client.clone();
        let repo = repo.id.clone();
        async move {
            client
                .list_operations_page(ListOperationsRequest {
                    repo,
                    filter: Some(filter),
                    page_size: 1000,
                    ..ListOperationsRequest::default()
                })
                .await
                .unwrap()
                .operations
        }
    };
    let kinds = list(OperationFilter {
        kinds: vec![
            OperationKind::MoveWorkspace.into(),
            OperationKind::CreateWorkspace.into(),
        ],
        ..OperationFilter::default()
    })
    .await;
    assert_eq!(kinds.len(), 2);
    let by_prefix = list(OperationFilter {
        workspaces: vec![ws.id[..6].to_owned()],
        ..OperationFilter::default()
    })
    .await;
    assert_eq!(by_prefix.len(), ours.len());
    let by_agent = list(OperationFilter {
        agents: vec![owner.to_owned()],
        ..OperationFilter::default()
    })
    .await;
    assert_eq!(by_agent.len(), ours.len());
    let reverse: String = ws
        .change_id
        .bytes()
        .take(8)
        .map(|digit| {
            let value = char::from(digit).to_digit(16).unwrap();
            char::from(b'z' - u8::try_from(value).unwrap())
        })
        .collect();
    let by_change = list(OperationFilter {
        changes: vec![reverse],
        ..OperationFilter::default()
    })
    .await;
    assert!(
        by_change
            .iter()
            .any(|op| op.kind() == OperationKind::CreateWorkspace)
    );
    assert!(
        by_change
            .iter()
            .all(|op| op.workspace_id == ws.id || op.workspace_id.is_empty())
    );

    // Time bounds.
    let create_op = all
        .iter()
        .find(|op| op.kind() == OperationKind::CreateWorkspace)
        .unwrap();
    let ended = create_op.ended_at.unwrap();
    let after = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            ended_after: Some(ended),
            page_size: 1000,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations;
    assert!(!after.iter().any(|op| op.id == create_op.id));
    let before = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            ended_before: Some(ended),
            page_size: 1000,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations;
    assert!(!before.iter().any(|op| op.workspace_id == ws.id));
    let with_effects = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            include_effects: true,
            page_size: 3,
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap()
        .operations;
    assert!(with_effects.iter().all(|op| op.effects.is_some()));
    let one = client
        .get_operation(&repo.id, &create_op.id[..12])
        .await
        .unwrap()
        .operation
        .unwrap();
    assert_eq!(one.id, create_op.id);
    assert!(
        one.effects
            .unwrap()
            .working_copies
            .iter()
            .any(|wc| wc.workspace_id == ws.id)
    );
    let error = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            filter: Some(OperationFilter {
                workspaces: vec!["ffffffffffffffff".into()],
                ..OperationFilter::default()
            }),
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).0, Code::NotFound);
    let error = client
        .list_operations_page(ListOperationsRequest {
            repo: repo.id.clone(),
            filter: Some(OperationFilter {
                changes: vec!["not-a-change".into()],
                ..OperationFilter::default()
            }),
            ..ListOperationsRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, reef_api::error::INVALID_CHANGE_ID);
    daemon.stop().await;
}

/// Asserts `listed` is `written` as `GetOperation` reports it.
fn assert_same(written: Option<&v1::Operation>, listed: &v1::Operation, what: &str) {
    let written = written.unwrap_or_else(|| panic!("{what}: nothing written"));
    assert_eq!(listed.id, written.id, "{what}");
    assert_eq!(listed.kind(), written.kind(), "{what}");
    assert_eq!(listed.snapshot_kind(), written.snapshot_kind(), "{what}");
    assert_eq!(listed.workspace_id, written.workspace_id, "{what}");
    assert_eq!(listed.agent, written.agent, "{what}");
    assert_eq!(listed.effects, written.effects, "{what}");
}

// 22 and 23
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "every write's operation, then the timing"
)]
async fn written_operations_carry_effects() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let get = async |id: &str| {
        client
            .get_operation(&repo.id, id)
            .await
            .unwrap()
            .operation
            .unwrap()
    };
    let created = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "effects@test".into(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap();
    let ws = created.workspace.clone().unwrap();
    let operation = created.operation.as_ref().unwrap();
    let effects = operation.effects.as_ref().unwrap();
    assert!(
        effects
            .changes
            .iter()
            .any(|c| c.kind() == v1::ChangeEffectKind::Created && c.commit_id == ws.commit_id)
    );
    assert!(
        effects
            .working_copies
            .iter()
            .any(|w| w.workspace_id == ws.id && w.previous_commit_id.is_empty())
    );
    assert_same(
        created.operation.as_ref(),
        &get(&operation.id).await,
        "create",
    );

    write(&ws, "effect.txt", "e\n");
    let snapshot = client.snapshot_workspace("", &ws.id, "").await.unwrap();
    let operation = snapshot.operation.as_ref().unwrap();
    assert!(
        operation
            .effects
            .as_ref()
            .unwrap()
            .changes
            .iter()
            .any(|c| c.kind() == v1::ChangeEffectKind::Rewritten
                && c.previous_commit_id == ws.commit_id)
    );
    assert_same(
        snapshot.operation.as_ref(),
        &get(&operation.id).await,
        "snapshot",
    );

    write(&ws, "before-move.txt", "m\n");
    let moved = client
        .move_workspace(MoveWorkspaceRequest {
            workspace: ws.id.clone(),
            target: Some(move_workspace_request::Target::NewOn("main".into())),
            ..MoveWorkspaceRequest::default()
        })
        .await
        .unwrap();
    let operation = moved.operation.as_ref().unwrap();
    assert!(
        operation
            .effects
            .as_ref()
            .unwrap()
            .working_copies
            .iter()
            .any(|w| w.workspace_id == ws.id)
    );
    assert_same(moved.operation.as_ref(), &get(&operation.id).await, "move");
    assert_same(
        moved.final_snapshot.as_ref(),
        &get(&moved.final_snapshot.as_ref().unwrap().id).await,
        "move snapshot",
    );

    let released = client
        .release_workspace("", &ws.id, true, false)
        .await
        .unwrap();
    let operation = released.operation.as_ref().unwrap();
    let effects = operation.effects.as_ref().unwrap();
    assert!(
        effects
            .working_copies
            .iter()
            .any(|w| w.workspace_id == ws.id && w.commit_id.is_empty())
    );
    assert!(
        effects
            .changes
            .iter()
            .any(|c| c.kind() == v1::ChangeEffectKind::Abandoned),
        "the moved-to change is empty: {effects:?}"
    );
    assert_same(
        released.operation.as_ref(),
        &get(&operation.id).await,
        "release",
    );

    // What effects add to a create, uncached (printed; the ceiling is
    // loose because timing tests flake under load).
    let mut times = Vec::new();
    for n in 0..20 {
        let created = client
            .create_workspace(CreateWorkspaceRequest {
                repo: repo.id.clone(),
                owner: format!("timed{n}@test"),
                ..CreateWorkspaceRequest::default()
            })
            .await
            .unwrap();
        let begun = Instant::now();
        let fresh = client
            .get_operation(&repo.id, &created.operation.unwrap().id)
            .await
            .unwrap();
        assert!(fresh.operation.unwrap().effects.is_some());
        times.push(begun.elapsed());
    }
    times.sort();
    let median = times[times.len() / 2];
    println!("GetOperation after a create (cached effects): median {median:?}");
    assert!(median < Duration::from_millis(250), "{median:?}");
    daemon.stop().await;
}

// 24
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "create, move and build, each with revsets"
)]
async fn create_and_move_take_revsets() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let new_on = |text: &str, owner: &str| CreateWorkspaceRequest {
        repo: repo.id.clone(),
        owner: owner.to_owned(),
        start: Some(create_workspace_request::Start::NewOn(text.to_owned())),
        ..CreateWorkspaceRequest::default()
    };
    let trunk = client
        .create_workspace(new_on("trunk()", "t@test"))
        .await
        .unwrap();
    let main = client
        .get_change(GetChangeRequest {
            repo: repo.id.clone(),
            revision: "main".into(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    let trunk_ws = trunk.workspace.unwrap();
    let trunk_change = client
        .get_change(GetChangeRequest {
            workspace: trunk_ws.id.clone(),
            revision: "@".into(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert_eq!(
        trunk_change.parent_commit_ids,
        std::slice::from_ref(&main.commit_id)
    );
    let root = client
        .create_workspace(new_on("main-", "root@test"))
        .await
        .unwrap();
    assert!(root.workspace.is_some());
    let by_change = client
        .create_workspace(new_on(&main.change_id[..12], "hex@test"))
        .await
        .unwrap();
    assert!(by_change.workspace.is_some());

    // A bookmark named after a change id's hex prefix starts on the
    // bookmark, not the change.
    let named = &trunk_change.change_id[..8];
    set_bookmark(&repo.root, named, &main.commit_id);
    let on_bookmark = client
        .create_workspace(new_on(named, "bm@test"))
        .await
        .unwrap();
    let ws = on_bookmark.workspace.unwrap();
    let change = client
        .get_change(GetChangeRequest {
            workspace: ws.id.clone(),
            revision: "@-".into(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert_eq!(change.commit_id, main.commit_id);

    // A divergent change is ambiguous.
    let first = trunk_ws;
    let second = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "twin@test".into(),
            start: Some(create_workspace_request::Start::Change(
                first.commit_id.clone(),
            )),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    write(&first, "a.txt", "a\n");
    write(&second, "b.txt", "b\n");
    for ws in [&first, &second] {
        client.snapshot_workspace("", &ws.id, "").await.unwrap();
    }
    let error = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "x@test".into(),
            start: Some(create_workspace_request::Start::Change(
                first.change_id.clone(),
            )),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (
            Code::InvalidArgument,
            reef_api::error::AMBIGUOUS_REVISION.to_owned()
        )
    );
    let error = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "x@test".into(),
            start: Some(create_workspace_request::Start::Change("@".into())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error).1, reef_api::error::WORKSPACE_REQUIRED);

    // A move to a child of the workspace's own snapshotted work.
    let mover = create(client, &repo, "mover@test").await;
    write(&mover, "work.txt", "w\n");
    let move_response = client
        .move_workspace(MoveWorkspaceRequest {
            workspace: mover.id.clone(),
            target: Some(move_workspace_request::Target::NewOn("@".into())),
            ..MoveWorkspaceRequest::default()
        })
        .await
        .unwrap();
    let snapshotted = move_response.final_snapshot.unwrap();
    let now = client
        .get_change(GetChangeRequest {
            workspace: mover.id.clone(),
            revision: "@-".into(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert_eq!(now.change_id, mover.change_id);
    assert!(Path::new(&mover.path).join("work.txt").is_file());
    assert_ne!(snapshotted.id, "");
    let listed = client
        .list_workspaces(ListWorkspacesRequest::default())
        .await
        .unwrap();
    assert!(listed.len() >= 6);

    // A store build's revision too: `trunk()` resolves (the fixture
    // declares no store, so the build stops there, naming the commit),
    // and a name that is nothing is the revision's error.
    let (code, _, message) = build_error(client, &repo, "trunk()").await;
    assert_eq!(code, Code::NotFound);
    assert!(message.contains(&main.commit_id), "{message}");
    let (_, reason, _) = build_error(client, &repo, "nope").await;
    assert_eq!(reason, reef_api::error::REVISION_NOT_FOUND);
    daemon.stop().await;
}

/// How a store build at `revision` fails: code, reason, message.
async fn build_error(
    client: &Client,
    repo: &SyntheticRepo,
    revision: &str,
) -> (Code, String, String) {
    match client
        .build_store(&repo.id, "deps", revision, Duration::from_secs(60), false)
        .await
    {
        Err(ClientError::Rpc {
            status,
            reason,
            message,
            ..
        }) => (status.code(), reason, message),
        Err(other) => panic!("{other:?}"),
        Ok(mut stream) => {
            let status = stream.message().await.unwrap_err();
            let detail = reef_api::error::detail(&status).unwrap_or_default();
            (status.code(), detail.reason, status.message().to_owned())
        }
    }
}

// #114 test 18
#[tokio::test(flavor = "multi_thread")]
async fn list_workspaces_counts_changed_files() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let busy = create(client, &repo, "busy@test").await;
    let fresh = create(client, &repo, "fresh@test").await;
    let gone = create(client, &repo, "gone@test").await;
    write(&busy, "one.txt", "1\n");
    write(&busy, "two.txt", "2\n");
    client
        .snapshot_workspace(&repo.id, &busy.id, "")
        .await
        .unwrap();
    client
        .release_workspace(&repo.id, &gone.id, false, false)
        .await
        .unwrap();
    let request = |counts: bool| ListWorkspacesRequest {
        repo: repo.id.clone(),
        changed_file_counts: counts,
        ..ListWorkspacesRequest::default()
    };

    let counted = client
        .list_workspaces_page(request(true))
        .await
        .unwrap()
        .workspaces;
    assert_eq!(counted.len(), 3);
    for ws in &counted {
        if ws.id == gone.id {
            assert_eq!(ws.changed_file_count, None, "released");
            continue;
        }
        let status = client
            .get_status(GetStatusRequest {
                skip_snapshot: true,
                ..status_request(ws)
            })
            .await
            .unwrap();
        let expected = status.workspace_status.unwrap().changed_file_count;
        assert_eq!(ws.changed_file_count, Some(expected), "{}", ws.id);
    }
    let by_id = |id: &str| counted.iter().find(|ws| ws.id == id).unwrap();
    assert_eq!(by_id(&busy.id).changed_file_count, Some(2));
    assert_eq!(by_id(&fresh.id).changed_file_count, Some(0));

    let plain = client.list_workspaces_page(request(false)).await.unwrap();
    assert!(
        plain
            .workspaces
            .iter()
            .all(|ws| ws.changed_file_count.is_none()),
        "{plain:?}"
    );

    // The flag is not part of the page token's binding.
    let first = client
        .list_workspaces_page(ListWorkspacesRequest {
            page_size: 1,
            ..request(true)
        })
        .await
        .unwrap();
    assert_eq!(first.workspaces.len(), 1);
    let rest = client
        .list_workspaces_page(ListWorkspacesRequest {
            page_size: 1,
            page_token: first.next_page_token,
            ..request(false)
        })
        .await
        .unwrap();
    assert_eq!(rest.workspaces.len(), 1);
}

/// A revset the web app reads (R2.19, R2.20), from its source so the two
/// cannot drift: the one double-quoted string on constant `name`'s line.
fn revset_from_web(name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../web/src/graph/revset.ts");
    let source = std::fs::read_to_string(&path).unwrap();
    let declared = format!("export const {name} ");
    let line = source
        .lines()
        .find(|line| line.starts_with(&declared))
        .unwrap_or_else(|| panic!("{name} is declared on one line"));
    let start = line.find('"').unwrap() + 1;
    let end = line.rfind('"').unwrap();
    line[start..end].to_owned()
}

/// R2.19 test 1: the smartlog's revset hangs each stack from its fork point
/// on the trunk spine, through indirect edges, and reading it in repository
/// scope records no operation.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one scripted history, then its log")]
async fn smartlog_revset() {
    use std::collections::BTreeMap;

    use jj_lib::backend::CommitId;
    use jj_lib::object_id::ObjectId as _;
    use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
    use jj_lib::ref_name::{RefName, RemoteName};
    use jj_lib::repo::Repo as _;

    let revset = revset_from_web("SMARTLOG_REVSET");
    assert_eq!(
        revset,
        "trunk() | working_copies() | ancestors(mutable(), 2)"
    );
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let base = Repo::open(&repo.root, &test_settings())
        .await
        .unwrap()
        .wc_change()
        .unwrap()
        .commit_id;

    // Trunk t1..t6 on the fixture's commit, `main` at t6; stack a1, a2 on
    // t6; b1 on t2; d1 on t4, named only by the remote bookmark dep@origin.
    let mut ids = BTreeMap::<&str, String>::new();
    with_tx(&repo.root, "smartlog history", |tx| {
        let store = tx.repo().store().clone();
        let tree = store
            .get_commit(&CommitId::try_from_hex(base.as_str()).unwrap())
            .unwrap()
            .tree();
        let mut commit = |name: &'static str, parent: &str, ids: &mut BTreeMap<&str, String>| {
            let written = pollster::block_on(
                tx.repo_mut()
                    .new_commit(vec![CommitId::try_from_hex(parent).unwrap()], tree.clone())
                    .set_description(name)
                    .write(),
            )
            .unwrap();
            ids.insert(name, written.id().hex());
        };
        let mut parent = base.as_str().to_owned();
        for name in ["t1", "t2", "t3", "t4", "t5", "t6"] {
            commit(name, &parent, &mut ids);
            parent.clone_from(&ids[name]);
        }
        let t2 = ids["t2"].clone();
        let t4 = ids["t4"].clone();
        let t6 = ids["t6"].clone();
        commit("a1", &t6, &mut ids);
        let a1 = ids["a1"].clone();
        commit("a2", &a1, &mut ids);
        commit("b1", &t2, &mut ids);
        commit("d1", &t4, &mut ids);
        tx.repo_mut().set_local_bookmark_target(
            RefName::new("main"),
            RefTarget::normal(CommitId::try_from_hex(&t6).unwrap()),
        );
        tx.repo_mut().set_remote_bookmark(
            RefName::new("dep").to_remote_symbol(RemoteName::new("origin")),
            RemoteRef {
                target: RefTarget::normal(CommitId::try_from_hex(&ids["d1"]).unwrap()),
                state: RemoteRefState::New,
            },
        );
    });

    // A workspace on a2, with an edit not yet snapshotted.
    let ws = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "smartlog@test".into(),
            start: Some(create_workspace_request::Start::Change(ids["a2"].clone())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    write(&ws, "unsnapshotted.txt", "x\n");
    let operations = |client: &Client| {
        let client = client.clone();
        let id = repo.id.clone();
        async move {
            client
                .list_operations_page(ListOperationsRequest {
                    repo: id,
                    page_size: 1000,
                    ..ListOperationsRequest::default()
                })
                .await
                .unwrap()
                .operations
                .len()
        }
    };
    let before = operations(client).await;

    for skip_snapshot in [true, false] {
        let log = client
            .list_changes(ListChangesRequest {
                repo: repo.id.clone(),
                revset: revset.clone(),
                skip_snapshot,
                ..ListChangesRequest::default()
            })
            .await
            .unwrap();
        let name = |commit: &str| {
            ids.iter().find(|(_, id)| id.as_str() == commit).map_or(
                if commit == base.as_str() {
                    "base"
                } else {
                    "other"
                },
                |(name, _)| name,
            )
        };
        let by_name: BTreeMap<&str, &v1::LogEntry> = log
            .iter()
            .map(|entry| (name(&entry.change.as_ref().unwrap().commit_id), entry))
            .collect();
        let mut names: Vec<&str> = by_name.keys().copied().collect();
        names.sort_unstable();
        // `base` is the root jj workspace's working copy, so in
        // `working_copies()`; the workspace's own working copy is `a2` or a
        // new commit on it.
        let wc = ws.commit_id.as_str();
        let expected: BTreeSet<&str> = ["a1", "a2", "b1", "base", "d1", "t2", "t4", "t6"]
            .into_iter()
            .chain((name(wc) == "other").then_some("other"))
            .collect();
        assert_eq!(names.into_iter().collect::<BTreeSet<_>>(), expected);
        let edges = |entry: &str| -> Vec<(&str, i32)> {
            by_name[entry]
                .edges
                .iter()
                .map(|edge| (name(&edge.commit_id), edge.kind))
                .collect()
        };
        let direct = v1::GraphEdgeKind::Direct as i32;
        let indirect = v1::GraphEdgeKind::Indirect as i32;
        assert_eq!(edges("t6"), [("t4", indirect)], "t5 left out");
        assert_eq!(edges("t4"), [("t2", indirect)], "t3 left out");
        assert_eq!(edges("t2"), [("base", indirect)], "t1 left out");
        assert_eq!(edges("a1"), [("t6", direct)]);
        assert_eq!(edges("a2"), [("a1", direct)]);
        assert_eq!(edges("b1"), [("t2", direct)]);
        assert_eq!(edges("d1"), [("t4", direct)]);
        // Children before parents: each loaded parent comes later in the log.
        let position: BTreeMap<&str, usize> = log
            .iter()
            .enumerate()
            .map(|(i, entry)| (entry.change.as_ref().unwrap().commit_id.as_str(), i))
            .collect();
        for (i, entry) in log.iter().enumerate() {
            for edge in &entry.edges {
                if let Some(&parent) = position.get(edge.commit_id.as_str()) {
                    assert!(parent > i, "{} before its child", name(&edge.commit_id));
                }
            }
        }
        // base's parent is outside the set: a missing edge, the stub.
        let missing = v1::GraphEdgeKind::Missing as i32;
        assert!(
            !by_name["base"].edges.is_empty()
                && by_name["base"]
                    .edges
                    .iter()
                    .all(|edge| edge.kind == missing),
            "{:?}",
            by_name["base"].edges
        );
        let change = |entry: &str| by_name[entry].change.as_ref().unwrap();
        assert!(change("t6").immutable && change("t2").immutable);
        assert!(
            !change("d1").immutable,
            "Reef's immutable_heads() leaves out remote-only bookmarks"
        );
        let workspace_change = by_name
            .values()
            .find(|entry| entry.change.as_ref().unwrap().commit_id == ws.commit_id)
            .unwrap();
        assert_eq!(
            workspace_change.change.as_ref().unwrap().workspace_ids,
            std::slice::from_ref(&ws.id)
        );
        // Repository scope never snapshots, with or without `skip_snapshot`.
        assert_eq!(
            operations(client).await,
            before,
            "skip_snapshot {skip_snapshot}"
        );
    }
    daemon.stop().await;
}

/// R2.20 test 1: what the workspaces page reads, in repository scope and
/// without a snapshot: both workspaces listed newest first with their
/// leases, their changes from `working_copies()`, an unsnapshotted file in a
/// disk walk but not in the count, no operation recorded, and one daemon
/// `LEASE` presence entry per workspace naming its owner.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "each read the page makes, then the stream"
)]
async fn fleet_panel_reads() {
    use reef_api::v1::{
        GetWorkspaceRequest, PresenceKind, SubscribePresenceRequest, WorkspaceState,
    };

    let revset = revset_from_web("WORKING_COPIES_REVSET");
    assert_eq!(revset, "working_copies()");
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let mut presence = client
        .subscribe_presence(SubscribePresenceRequest::default())
        .await
        .unwrap();
    let first = create(client, &repo, "first@test").await;
    let second = create(client, &repo, "second@test").await;
    write(&first, "unsnapshotted.txt", "x\n");
    let operation_count = || async {
        client
            .list_operations_page(ListOperationsRequest {
                repo: repo.id.clone(),
                page_size: 1000,
                ..ListOperationsRequest::default()
            })
            .await
            .unwrap()
            .operations
            .len()
    };
    let before = operation_count().await;

    let listed = client
        .list_workspaces_page(ListWorkspacesRequest {
            repo: repo.id.clone(),
            states: vec![
                WorkspaceState::Active.into(),
                WorkspaceState::Releasing.into(),
            ],
            changed_file_counts: true,
            page_size: 1000,
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap()
        .workspaces;
    let ids: Vec<_> = listed.iter().map(|ws| ws.id.as_str()).collect();
    assert_eq!(ids, [second.id.as_str(), first.id.as_str()], "newest first");
    for ws in &listed {
        let lease = ws.lease.as_ref().unwrap();
        assert_eq!(lease.state(), v1::LeaseState::Active, "{}", ws.id);
        assert!(lease.heartbeat_every.is_some(), "{}", ws.id);
        // Never renewed: the page reads it as "not heartbeating".
        assert_eq!(lease.last_heartbeat_at, lease.issued_at, "{}", ws.id);
    }
    assert_eq!(listed[1].changed_file_count, Some(0), "not snapshotted");

    let entries = client
        .list_changes(ListChangesRequest {
            repo: repo.id.clone(),
            revset,
            skip_snapshot: true,
            ..ListChangesRequest::default()
        })
        .await
        .unwrap();
    for ws in [&first, &second] {
        let change = entries
            .iter()
            .filter_map(|entry| entry.change.as_ref())
            .find(|change| change.workspace_ids.contains(&ws.id))
            .unwrap_or_else(|| panic!("{}'s change in {entries:?}", ws.id));
        assert_eq!(change.change_id, ws.change_id);
    }

    let got = client
        .get_workspace(GetWorkspaceRequest {
            repo: repo.id.clone(),
            workspace: first.id.clone(),
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap();
    let disk = got.disk_usage.expect("measured");
    assert!(disk.files_changed >= 1, "{disk:?}");
    assert!(disk.allocated_bytes > 0, "{disk:?}");

    assert_eq!(
        operation_count().await,
        before,
        "the reads recorded nothing"
    );
    let commit = client
        .list_workspaces_page(ListWorkspacesRequest {
            repo: repo.id.clone(),
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap()
        .workspaces
        .into_iter()
        .find(|ws| ws.id == first.id)
        .unwrap()
        .commit_id;
    assert_eq!(commit, first.commit_id, "nothing snapshotted");

    // The stream's set, then the creates' entries.
    let mut leases = BTreeSet::new();
    let deadline = Instant::now() + Duration::from_secs(10);
    while leases.len() < 2 {
        let left = deadline.saturating_duration_since(Instant::now());
        let frame = tokio::time::timeout(left, presence.message())
            .await
            .expect("lease entries in time")
            .unwrap()
            .unwrap();
        for event in frame.events {
            let Some(entry) = event.presence else {
                continue;
            };
            if event.ended || entry.kind() != PresenceKind::Lease {
                continue;
            }
            assert!(entry.by_daemon, "{entry:?}");
            assert!(entry.lease.is_some(), "{entry:?}");
            let owner = if entry.workspace_id == first.id {
                "first@test"
            } else {
                "second@test"
            };
            assert_eq!(entry.agent, owner, "{entry:?}");
            leases.insert(entry.workspace_id);
        }
    }
    assert_eq!(
        leases,
        BTreeSet::from([first.id.clone(), second.id.clone()])
    );
}
