//! `reef.api.v1.FileService` through the daemon (R2.04, tests 9 to 16):
//! reading and listing at a revision with the snapshot rule and pinned
//! pages, and edits without a workspace, with the live workspaces they
//! move, their refusals, concurrency, operations and undo. Issue #80's
//! tests 6, 7 and 9: edits a workspace makes during an edit are carried
//! onto its commit.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeMap;
use std::future::Future;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, ChangeEffectKind, CreateWorkspaceRequest, EditFilesRequest, EditFilesResponse, EntryType,
    FileChangeKind, FileDelete, FileEdit, GetChangeRequest, GetDiffRequest, ListFilesRequest,
    ListOperationsRequest, OperationFilter, OperationKind, ReadFileRequest, SnapshotKind,
    TextReplace, UndoOperationRequest, create_workspace_request, edit_files_request, file_edit,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::snapshot::SnapshotOutcome;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_testing::synth::Preset;
use tonic::Code;

/// A daemon whose watcher never snapshots on its own, over a small
/// synthetic repository.
async fn fixture() -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
    })
    .await
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

fn read(workspace: &v1::Workspace, path: &str) -> Option<String> {
    std::fs::read_to_string(Path::new(&workspace.path).join(path)).ok()
}

async fn get(client: &Client, repo: &SyntheticRepo, ws: &v1::Workspace) -> v1::Workspace {
    client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn snapshot(client: &Client, repo: &SyntheticRepo, ws: &v1::Workspace) {
    client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
}

fn reason(error: &ClientError) -> (Code, String) {
    match error {
        ClientError::Rpc { status, reason, .. } => (status.code(), reason.clone()),
        other => panic!("{other:?}"),
    }
}

fn metadata(error: &ClientError) -> BTreeMap<String, String> {
    match error {
        ClientError::Rpc { metadata, .. } => metadata.clone(),
        other => panic!("{other:?}"),
    }
}

fn read_request(
    repo: &SyntheticRepo,
    ws: Option<&v1::Workspace>,
    revision: &str,
    path: &str,
) -> ReadFileRequest {
    ReadFileRequest {
        repo: repo.id.clone(),
        workspace: ws.map(|ws| ws.id.clone()).unwrap_or_default(),
        revision: revision.to_owned(),
        path: path.to_owned(),
        ..ReadFileRequest::default()
    }
}

async fn file(client: &Client, repo: &SyntheticRepo, revision: &str, path: &str) -> Option<String> {
    match client
        .read_file_to_end(ReadFileRequest {
            skip_snapshot: true,
            ..read_request(repo, None, revision, path)
        })
        .await
    {
        Ok(response) => Some(String::from_utf8(response.content).unwrap()),
        Err(error) if reason(&error).1 == "PATH_NOT_FOUND" => None,
        Err(error) => panic!("{error:?}"),
    }
}

fn content(path: &str, text: &str) -> FileEdit {
    FileEdit {
        path: path.to_owned(),
        action: Some(file_edit::Action::Content(text.as_bytes().to_vec())),
        executable: None,
    }
}

fn replace(path: &str, old: &str, new: &str) -> FileEdit {
    FileEdit {
        path: path.to_owned(),
        action: Some(file_edit::Action::Replace(TextReplace {
            old_text: old.to_owned(),
            new_text: new.to_owned(),
            all: false,
        })),
        executable: None,
    }
}

fn delete(path: &str) -> FileEdit {
    FileEdit {
        path: path.to_owned(),
        action: Some(file_edit::Action::Delete(FileDelete {})),
        executable: None,
    }
}

fn copy(path: &str, from: &str) -> FileEdit {
    FileEdit {
        path: path.to_owned(),
        action: Some(file_edit::Action::CopyFrom(from.to_owned())),
        executable: None,
    }
}

fn rewrite(
    repo: &SyntheticRepo,
    ws: Option<&v1::Workspace>,
    revision: &str,
    edits: Vec<FileEdit>,
) -> EditFilesRequest {
    EditFilesRequest {
        repo: repo.id.clone(),
        workspace: ws.map(|ws| ws.id.clone()).unwrap_or_default(),
        target: Some(edit_files_request::Target::Revision(revision.to_owned())),
        edits,
        ..EditFilesRequest::default()
    }
}

fn new_on(
    repo: &SyntheticRepo,
    ws: Option<&v1::Workspace>,
    revision: &str,
    edits: Vec<FileEdit>,
) -> EditFilesRequest {
    EditFilesRequest {
        target: Some(edit_files_request::Target::NewOn(revision.to_owned())),
        ..rewrite(repo, ws, "", edits)
    }
}

async fn edit(client: &Client, request: EditFilesRequest) -> EditFilesResponse {
    client.edit_files(request).await.unwrap()
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

async fn all_operations(client: &Client, repo: &SyntheticRepo) -> usize {
    operations(client, repo, OperationFilter::default())
        .await
        .len()
}

async fn trunk(client: &Client, repo: &SyntheticRepo) -> v1::Change {
    client
        .get_change(GetChangeRequest {
            repo: repo.id.clone(),
            revision: "trunk()".to_owned(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

/// Runs `rpc` with the write held between its write and its follow-ups
/// (`Scheduler::hold_follows`), making the workspace's late edits with
/// `late` meanwhile.
async fn at_follow_hold<T: Send + 'static>(
    daemon: &TestDaemon,
    rpc: impl Future<Output = T> + Send + 'static,
    late: impl FnOnce(),
) -> T {
    let scheduler = daemon.handle.state().manager.scheduler();
    let hold = scheduler.hold_follows().await;
    let task = tokio::spawn(rpc);
    let begun = Instant::now();
    while scheduler.held_follows() == 0 {
        assert!(
            begun.elapsed() < Duration::from_secs(30),
            "the write never reached its follow-ups"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    late();
    drop(hold);
    task.await.unwrap()
}

/// The visible commits of `ws`'s change.
async fn change_commits(client: &Client, repo: &SyntheticRepo, change_id: &str) -> Vec<String> {
    client
        .list_changes(v1::ListChangesRequest {
            repo: repo.id.clone(),
            revset: "all()".to_owned(),
            skip_snapshot: true,
            ..v1::ListChangesRequest::default()
        })
        .await
        .unwrap()
        .into_iter()
        .filter_map(|entry| entry.change)
        .filter(|change| change.change_id == change_id)
        .map(|change| change.commit_id)
        .collect()
}

/// `ws`'s operations recorded after `op`, newest first.
async fn operations_after(
    client: &Client,
    repo: &SyntheticRepo,
    ws: &v1::Workspace,
    op: &str,
) -> Vec<v1::Operation> {
    let ops = operations(
        client,
        repo,
        OperationFilter {
            workspaces: vec![ws.id.clone()],
            ..OperationFilter::default()
        },
    )
    .await;
    let at = ops
        .iter()
        .position(|listed| listed.id == op)
        .expect("the operation is W's");
    ops[..at].to_vec()
}

/// The commit ids of every divergent change, by jj's `divergent()`.
async fn divergent_changes(client: &Client, repo: &SyntheticRepo) -> Vec<String> {
    client
        .list_changes(v1::ListChangesRequest {
            repo: repo.id.clone(),
            revset: "divergent()".to_owned(),
            skip_snapshot: true,
            ..v1::ListChangesRequest::default()
        })
        .await
        .unwrap()
        .into_iter()
        .filter_map(|entry| entry.change)
        .map(|change| change.commit_id)
        .collect()
}

/// Whether `id` is lowercase hex of `len` digits.
fn is_hex(id: &str, len: usize) -> bool {
    id.len() == len
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn read_file_at_revisions() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "dir/a.txt", "one\n");
    snapshot(client, &repo, &w).await;
    let snapshotted = operations(client, &repo, OperationFilter::default()).await[0]
        .id
        .clone();
    write(&w, "dir/a.txt", "two\n");

    let skipped = client
        .read_file(ReadFileRequest {
            skip_snapshot: true,
            ..read_request(&repo, Some(&w), "@", "dir/a.txt")
        })
        .await
        .unwrap();
    assert_eq!(skipped.content, b"one\n");
    let before = all_operations(client, &repo).await;
    let at = client
        .read_file(ReadFileRequest {
            at_operation: snapshotted.clone(),
            ..read_request(&repo, Some(&w), "@", "dir/a.txt")
        })
        .await
        .unwrap();
    assert_eq!(at.content, b"one\n");
    assert_eq!(at.at_operation, snapshotted);
    assert_eq!(
        all_operations(client, &repo).await,
        before,
        "no snapshot at an operation"
    );

    let fresh = client
        .read_file(read_request(&repo, Some(&w), "@", "dir/a.txt"))
        .await
        .unwrap();
    assert_eq!(fresh.content, b"two\n");
    assert_eq!(fresh.size, 4);
    assert_eq!(fresh.entry.as_ref().unwrap().r#type(), EntryType::File);
    assert_eq!(fresh.entry.as_ref().unwrap().path, "dir/a.txt");
    assert!(
        is_hex(&fresh.commit_id, 40) && is_hex(&fresh.at_operation, 128),
        "{fresh:?}"
    );
    let latest = &operations(client, &repo, OperationFilter::default()).await[0];
    assert_eq!(
        latest.kind(),
        OperationKind::Snapshot,
        "the read snapshotted first"
    );
    assert_eq!(latest.id, fresh.at_operation);

    let cases = [
        (
            read_request(&repo, Some(&w), "", "dir/a.txt"),
            Code::InvalidArgument,
            "REVISION_REQUIRED",
        ),
        (
            read_request(&repo, None, "@", "dir/a.txt"),
            Code::InvalidArgument,
            "WORKSPACE_REQUIRED",
        ),
        (
            read_request(&repo, Some(&w), "@", "dir"),
            Code::FailedPrecondition,
            "NOT_A_FILE",
        ),
        (
            read_request(&repo, Some(&w), "@", "nope"),
            Code::NotFound,
            "PATH_NOT_FOUND",
        ),
        (
            read_request(&repo, Some(&w), "@", "../x"),
            Code::InvalidArgument,
            "INVALID_PATH",
        ),
        (
            read_request(&repo, Some(&w), "@", ""),
            Code::InvalidArgument,
            "INVALID_PATH",
        ),
    ];
    for (request, code, expected) in cases {
        let error = client.read_file(request).await.unwrap_err();
        assert_eq!(reason(&error), (code, expected.to_owned()), "{error:?}");
        if expected == "NOT_A_FILE" {
            assert_eq!(metadata(&error)["type"], "ENTRY_TYPE_DIRECTORY");
            assert_ne!(error.advice().unwrap().hint, "");
        }
        if expected == "PATH_NOT_FOUND" {
            let metadata = metadata(&error);
            assert_eq!(metadata["path"], "nope");
            assert_eq!(metadata["commit_id"], fresh.commit_id);
        }
    }
    daemon.stop().await;
}

// 10
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines, reason = "one fixture, every listing case")]
async fn list_files_pages() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    for i in 0..250 {
        write(&w, &format!("many/d{}/f{i:03}.txt", i % 7), "x");
    }
    write(&w, "many/top.txt", "t");
    snapshot(client, &repo, &w).await;
    let request = ListFilesRequest {
        workspace: w.id.clone(),
        revision: "@".to_owned(),
        path: "many".to_owned(),
        recursive: true,
        ..ListFilesRequest::default()
    };
    let whole = client
        .list_files_page(ListFilesRequest {
            page_size: 1000,
            ..request.clone()
        })
        .await
        .unwrap();
    assert_eq!(whole.entries.len(), 251);
    assert_eq!(whole.next_page_token, "");
    assert!(
        whole
            .entries
            .iter()
            .all(|entry| entry.r#type() == EntryType::File)
    );

    let first = client
        .list_files_page(ListFilesRequest {
            page_size: 100,
            ..request.clone()
        })
        .await
        .unwrap();
    assert_eq!(first.entries.len(), 100);
    // An edit between pages does not change the later pages: they read at
    // the operation the first page read.
    edit(
        client,
        rewrite(
            &repo,
            Some(&w),
            "@",
            vec![content("many/d0/aaa.txt", "late")],
        ),
    )
    .await;
    let mut paged = first.entries.clone();
    let mut token = first.next_page_token.clone();
    while !token.is_empty() {
        let page = client
            .list_files_page(ListFilesRequest {
                page_size: 100,
                page_token: token,
                ..request.clone()
            })
            .await
            .unwrap();
        assert_eq!(page.commit_id, first.commit_id);
        paged.extend(page.entries);
        token = page.next_page_token;
    }
    assert_eq!(paged, whole.entries);

    let other = client
        .list_files_page(ListFilesRequest {
            path: "elsewhere".to_owned(),
            page_size: 100,
            page_token: first.next_page_token.clone(),
            ..request.clone()
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&other),
        (Code::InvalidArgument, "INVALID_PAGE_TOKEN".to_owned())
    );

    // Not recursive: the directory's own entries, directories among them.
    let own = client
        .list_files(ListFilesRequest {
            recursive: false,
            ..request.clone()
        })
        .await
        .unwrap();
    let names: Vec<(&str, EntryType)> = own
        .entries
        .iter()
        .map(|entry| (entry.path.as_str(), entry.r#type()))
        .collect();
    assert_eq!(names.len(), 8, "{names:?}");
    assert_eq!(names[0], ("many/d0", EntryType::Directory));
    assert_eq!(names[7], ("many/top.txt", EntryType::File));

    let error = client
        .list_files_page(ListFilesRequest {
            path: "many/top.txt".to_owned(),
            ..request
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "NOT_A_DIRECTORY".to_owned())
    );
    assert_eq!(metadata(&error)["type"], "ENTRY_TYPE_FILE");
    daemon.stop().await;
}

// 11
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines, reason = "one change, edited every way")]
async fn edit_files_without_a_workspace() {
    let (daemon, repo) = fixture().await;
    let client = daemon.client.clone().with_agent("editor").unwrap();
    let client = &client;
    let workspaces_before = client
        .list_workspaces(v1::ListWorkspacesRequest {
            repo: repo.id.clone(),
            ..v1::ListWorkspacesRequest::default()
        })
        .await
        .unwrap()
        .len();
    let trunk = trunk(client, &repo).await;

    let mut latencies = Vec::new();
    let started = Instant::now();
    let created = edit(
        client,
        new_on(
            &repo,
            None,
            "trunk()",
            vec![
                content("new/a.txt", "alpha\n"),
                content("new/b.txt", "beta\n"),
            ],
        ),
    )
    .await;
    latencies.push(started.elapsed());
    let change = created.change.unwrap();
    assert_eq!(
        change.parent_commit_ids,
        std::slice::from_ref(&trunk.commit_id)
    );
    let operation = created.operation.unwrap();
    assert_eq!(operation.kind(), OperationKind::Edit);
    assert_eq!(operation.agent, "editor");
    assert_eq!(operation.attributes["reef.kind"], "edit");
    let effects = operation.effects.unwrap();
    assert!(
        effects
            .changes
            .iter()
            .any(|effect| effect.kind() == ChangeEffectKind::Created
                && effect.commit_id == change.commit_id),
        "{effects:?}"
    );
    assert_eq!(
        file(client, &repo, &change.commit_id, "new/a.txt")
            .await
            .as_deref(),
        Some("alpha\n")
    );
    assert_eq!(
        client
            .list_workspaces(v1::ListWorkspacesRequest {
                repo: repo.id.clone(),
                ..v1::ListWorkspacesRequest::default()
            })
            .await
            .unwrap()
            .len(),
        workspaces_before
    );

    // By change id, checked against the commit just read: a rewrite that
    // keeps the change.
    let started = Instant::now();
    let rewritten = edit(
        client,
        EditFilesRequest {
            expected_commit_id: change.commit_id.clone(),
            description: Some("edited".to_owned()),
            ..rewrite(
                &repo,
                None,
                &change.change_id,
                vec![
                    copy("new/c.txt", "new/b.txt"),
                    delete("new/b.txt"),
                    replace("new/a.txt", "alpha", "ALPHA"),
                ],
            )
        },
    )
    .await;
    latencies.push(started.elapsed());
    let after = rewritten.change.unwrap();
    assert_eq!(after.change_id, change.change_id);
    assert_ne!(after.commit_id, change.commit_id);
    assert_eq!(after.description, "edited\n");
    assert_eq!(
        rewritten.operation.unwrap().effects.unwrap().changes[0].kind(),
        ChangeEffectKind::Rewritten
    );
    assert_eq!(
        file(client, &repo, &after.commit_id, "new/a.txt")
            .await
            .as_deref(),
        Some("ALPHA\n")
    );
    // A stale id is a mismatch naming both.
    let stale = client
        .edit_files(EditFilesRequest {
            expected_commit_id: change.commit_id.clone(),
            ..rewrite(
                &repo,
                None,
                &change.change_id,
                vec![content("new/d.txt", "d")],
            )
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&stale),
        (
            Code::FailedPrecondition,
            "EXPECTED_COMMIT_MISMATCH".to_owned()
        )
    );
    let found = metadata(&stale);
    assert_eq!(
        (
            found["expected_commit_id"].as_str(),
            found["commit_id"].as_str()
        ),
        (change.commit_id.as_str(), after.commit_id.as_str())
    );

    // The diff reports the copy and delete as a rename.
    let diff = client
        .get_diff(GetDiffRequest {
            repo: repo.id.clone(),
            from_revision: change.commit_id.clone(),
            to_revision: after.commit_id.clone(),
            ..GetDiffRequest::default()
        })
        .await
        .unwrap();
    let kinds: BTreeMap<&str, FileChangeKind> = diff
        .files
        .iter()
        .map(|file| {
            let change = file.change.as_ref().unwrap();
            (change.path.as_str(), change.kind())
        })
        .collect();
    assert_eq!(
        kinds.get("new/c.txt"),
        Some(&FileChangeKind::Renamed),
        "{kinds:?}"
    );
    assert_eq!(kinds.get("new/a.txt"), Some(&FileChangeKind::Modified));

    // The same content again records nothing.
    let again = edit(
        client,
        rewrite(
            &repo,
            None,
            &after.commit_id,
            vec![content("new/a.txt", "ALPHA\n")],
        ),
    )
    .await;
    assert!(again.operation.is_none());
    assert_eq!(again.change.unwrap().commit_id, after.commit_id);
    for _ in 0..18 {
        let started = Instant::now();
        edit(
            client,
            new_on(&repo, None, "trunk()", vec![content("x.txt", "x")]),
        )
        .await;
        latencies.push(started.elapsed());
    }
    latencies.sort();
    println!(
        "EditFiles without a workspace: median {:?} over {}",
        latencies[latencies.len() / 2],
        latencies.len()
    );
    daemon.stop().await;
}

// 12
#[tokio::test(flavor = "multi_thread")]
#[allow(
    clippy::too_many_lines,
    reason = "two workspaces through one edit, then a new change"
)]
async fn edit_files_moves_workspaces() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "a.txt", "hello world\n");
    snapshot(client, &repo, &w).await;
    let w_row = get(client, &repo, &w).await;
    let v = create(
        client,
        &repo,
        "v@test",
        Some(create_workspace_request::Start::NewOn(
            w_row.commit_id.clone(),
        )),
    )
    .await;
    write(&v, "child.txt", "child\n");
    snapshot(client, &repo, &v).await;
    // Unsnapshotted edits in both; the replacement's text is only in W's.
    write(&w, "a.txt", "hello unsaved world\n");
    write(&v, "v-late.txt", "late\n");

    let started = Instant::now();
    let response = edit(
        client,
        rewrite(
            &repo,
            Some(&w),
            "@",
            vec![
                content("b.txt", "b\n"),
                replace("a.txt", "unsaved", "saved"),
            ],
        ),
    )
    .await;
    let elapsed = started.elapsed();
    println!("EditFiles with a workspace that follows: {elapsed:?}");
    let operation = response.operation.unwrap();
    let w_ops = operations(
        client,
        &repo,
        OperationFilter {
            workspaces: vec![w.id.clone()],
            ..OperationFilter::default()
        },
    )
    .await;
    assert_eq!(w_ops[0].id, operation.id);
    assert_eq!(w_ops[0].kind(), OperationKind::Edit);
    assert!(
        w_ops.iter().any(|op| op.kind() == OperationKind::Snapshot
            && op.snapshot_kind() == SnapshotKind::Explicit),
        "{w_ops:?}"
    );
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
    // Both were snapshotted before the edit, descendants first.
    let all = operations(client, &repo, OperationFilter::default()).await;
    let at = all.iter().position(|op| op.id == operation.id).unwrap();
    let before: Vec<(&str, OperationKind)> = all[at + 1..at + 3]
        .iter()
        .map(|op| (op.workspace_id.as_str(), op.kind()))
        .collect();
    assert_eq!(
        before,
        [
            (w.id.as_str(), OperationKind::Snapshot),
            (v.id.as_str(), OperationKind::Snapshot)
        ]
    );

    // W's directory took the edit through a checkout.
    assert_eq!(read(&w, "b.txt").as_deref(), Some("b\n"));
    assert_eq!(read(&w, "a.txt").as_deref(), Some("hello saved world\n"));
    let w_after = get(client, &repo, &w).await;
    assert_eq!(w_after.change_id, w.change_id);
    assert_eq!(w_after.commit_id, response.change.unwrap().commit_id);
    assert!(!w_after.divergent);
    // V was snapshotted first, then rebased: it has the edit and keeps its
    // own.
    assert_eq!(read(&v, "b.txt").as_deref(), Some("b\n"));
    assert_eq!(read(&v, "v-late.txt").as_deref(), Some("late\n"));
    assert_eq!(read(&v, "child.txt").as_deref(), Some("child\n"));
    let v_after = get(client, &repo, &v).await;
    assert_eq!(v_after.change_id, v.change_id);
    assert!(!v_after.divergent);

    // A new change on `@` leaves W where it is.
    let made = edit(
        client,
        new_on(&repo, Some(&w), "@", vec![content("c.txt", "c\n")]),
    )
    .await;
    let made = made.change.unwrap();
    assert_eq!(
        made.parent_commit_ids,
        std::slice::from_ref(&w_after.commit_id)
    );
    let w_still = get(client, &repo, &w).await;
    assert_eq!(w_still.commit_id, w_after.commit_id);
    assert_eq!(read(&w, "c.txt"), None);
    daemon.stop().await;
}

// 13
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines, reason = "one fixture, every refusal")]
async fn edit_files_refusals() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "a.txt", "a\n");
    snapshot(client, &repo, &w).await;
    let old = get(client, &repo, &w).await.commit_id;
    write(&w, "a.txt", "a again\n");
    snapshot(client, &repo, &w).await;

    let immutable = client
        .edit_files(rewrite(&repo, None, "trunk()", vec![content("x", "x")]))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&immutable),
        (Code::FailedPrecondition, "IMMUTABLE_REVISION".to_owned())
    );
    edit(
        client,
        new_on(&repo, None, "trunk()", vec![content("x", "x")]),
    )
    .await;
    for request in [
        rewrite(&repo, None, &old, vec![content("x", "x")]),
        new_on(&repo, None, &old, vec![content("x", "x")]),
    ] {
        let hidden = client.edit_files(request).await.unwrap_err();
        assert_eq!(
            reason(&hidden),
            (Code::FailedPrecondition, "HIDDEN_REVISION".to_owned())
        );
    }
    let none = client
        .edit_files(EditFilesRequest {
            target: None,
            ..rewrite(&repo, Some(&w), "@", vec![])
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&none),
        (Code::InvalidArgument, "REVISION_REQUIRED".to_owned())
    );
    let bad_id = client
        .edit_files(EditFilesRequest {
            expected_commit_id: "abc".to_owned(),
            ..rewrite(&repo, Some(&w), "@", vec![content("x", "x")])
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&bad_id),
        (Code::InvalidArgument, "INVALID_COMMIT_ID".to_owned())
    );

    // A request wrong against the current tree of a commit no workspace
    // is on records nothing.
    let lone = edit(
        client,
        new_on(&repo, None, "trunk()", vec![content("a.txt", "a\n")]),
    )
    .await
    .change
    .unwrap();
    let before = all_operations(client, &repo).await;
    let missing = client
        .edit_files(rewrite(
            &repo,
            None,
            &lone.commit_id,
            vec![content("ok.txt", "1"), replace("a.txt", "absent text", "y")],
        ))
        .await
        .unwrap_err();
    assert_eq!(
        reason(&missing),
        (Code::FailedPrecondition, "TEXT_NOT_FOUND".to_owned())
    );
    let found = metadata(&missing);
    assert_eq!(
        (found["edit_index"].as_str(), found["path"].as_str()),
        ("1", "a.txt")
    );
    assert_eq!(
        all_operations(client, &repo).await,
        before,
        "nothing recorded"
    );
    // On a workspace's commit, the edits see its files on disk: it is
    // snapshotted first, and that snapshot stands when the edit fails.
    write(&w, "unsaved.txt", "u\n");
    let missing = client
        .edit_files(rewrite(
            &repo,
            Some(&w),
            "@",
            vec![replace("a.txt", "absent text", "y")],
        ))
        .await
        .unwrap_err();
    assert_eq!(reason(&missing).1, "TEXT_NOT_FOUND");
    let ops = operations(client, &repo, OperationFilter::default()).await;
    assert_eq!(ops.len(), before + 1);
    assert_eq!(ops[0].kind(), OperationKind::Snapshot);
    let before = ops.len();

    // Each per-edit refusal names the edit.
    let cases: Vec<(Vec<FileEdit>, Code, &str, &str)> = vec![
        (
            vec![content(".git/config", "x")],
            Code::InvalidArgument,
            "INVALID_PATH",
            ".git/config",
        ),
        (
            vec![delete("nope")],
            Code::NotFound,
            "PATH_NOT_FOUND",
            "nope",
        ),
        (
            vec![content("a.txt/x", "x")],
            Code::FailedPrecondition,
            "NOT_A_DIRECTORY",
            "a.txt",
        ),
        (
            vec![FileEdit {
                path: "a.txt".into(),
                action: None,
                executable: None,
            }],
            Code::InvalidArgument,
            "INVALID_EDIT",
            "a.txt",
        ),
        (
            vec![content("k", "aa aa"), replace("k", "aa", "b")],
            Code::FailedPrecondition,
            "TEXT_NOT_UNIQUE",
            "k",
        ),
    ];
    for (edits, code, expected, path) in cases {
        let index = (edits.len() - 1).to_string();
        let error = client
            .edit_files(rewrite(&repo, Some(&w), "@", edits))
            .await
            .unwrap_err();
        assert_eq!(reason(&error), (code, expected.to_owned()), "{error:?}");
        let found = metadata(&error);
        assert_eq!(found["edit_index"], index, "{expected}");
        assert_eq!(found["path"], path, "{expected}");
    }
    assert_eq!(
        all_operations(client, &repo).await,
        before,
        "nothing recorded"
    );

    // Too large: the client refuses to send it; the daemon refuses it too.
    let big = rewrite(
        &repo,
        Some(&w),
        "@",
        vec![content("big", &"x".repeat(5 << 20))],
    );
    assert!(matches!(
        client.edit_files(big.clone()).await,
        Err(ClientError::TooLarge { .. })
    ));
    let mut raw = v1::file_service_client::FileServiceClient::new(
        raw_channel(daemon.handle.socket_path()).await,
    );
    let error = raw.edit_files(big).await.unwrap_err();
    assert_eq!(error.code(), Code::OutOfRange, "{error:?}");

    // A released workspace cannot be named.
    client
        .release_workspace(&repo.id, &w.id, true, false)
        .await
        .unwrap();
    let released = client
        .edit_files(new_on(&repo, Some(&w), "trunk()", vec![content("x", "x")]))
        .await
        .unwrap_err();
    assert_eq!(reason(&released).0, Code::FailedPrecondition);
    daemon.stop().await;
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_edits() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let change = edit(
        client,
        new_on(&repo, None, "trunk()", vec![content("base.txt", "b")]),
    )
    .await
    .change
    .unwrap();
    let edits = (0..20).map(|i| {
        let client = client.clone();
        let request = rewrite(
            &repo,
            None,
            &change.change_id,
            vec![content(&format!("f{i}.txt"), "x")],
        );
        tokio::spawn(async move { client.edit_files(request).await })
    });
    for task in futures::future::join_all(edits).await {
        task.unwrap().unwrap();
    }
    let now = client
        .get_change(GetChangeRequest {
            repo: repo.id.clone(),
            revision: change.change_id.clone(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert!(!now.divergent);
    for i in 0..20 {
        assert!(
            file(client, &repo, &now.commit_id, &format!("f{i}.txt"))
                .await
                .is_some(),
            "f{i}"
        );
    }

    let expected = now.commit_id.clone();
    let edits = (0..20).map(|i| {
        let client = client.clone();
        let request = EditFilesRequest {
            expected_commit_id: expected.clone(),
            ..rewrite(
                &repo,
                None,
                &change.change_id,
                vec![content(&format!("g{i}.txt"), "x")],
            )
        };
        tokio::spawn(async move { client.edit_files(request).await })
    });
    let (mut won, mut lost) = (0, 0);
    for task in futures::future::join_all(edits).await {
        match task.unwrap() {
            Ok(_) => won += 1,
            Err(error) => {
                assert_eq!(reason(&error).1, "EXPECTED_COMMIT_MISMATCH");
                lost += 1;
            }
        }
    }
    assert_eq!((won, lost), (1, 19));
    daemon.stop().await;
}

// 15; tightened by issue #80: edits made during each write are carried
// onto its commit.
#[tokio::test(flavor = "multi_thread")]
async fn edit_while_the_workspace_writes() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (w, stop) = (w.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut n = 0u32;
            let deadline = Instant::now() + Duration::from_secs(2);
            while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                n += 1;
                write(&w, "busy.txt", &format!("{n}\n"));
                std::thread::sleep(Duration::from_millis(5));
            }
            n
        })
    };
    for i in 0..20 {
        edit(
            client,
            rewrite(
                &repo,
                Some(&w),
                "@",
                vec![content(&format!("api/{i}.txt"), "x")],
            ),
        )
        .await;
    }
    stop.store(true, Ordering::SeqCst);
    let last = writer.join().unwrap();
    snapshot(client, &repo, &w).await;
    let row = get(client, &repo, &w).await;
    // Every commit of W's change that is visible.
    let commits: Vec<String> = client
        .list_changes(v1::ListChangesRequest {
            repo: repo.id.clone(),
            revset: "all()".to_owned(),
            skip_snapshot: true,
            ..v1::ListChangesRequest::default()
        })
        .await
        .unwrap()
        .into_iter()
        .filter_map(|entry| entry.change)
        .filter(|change| change.change_id == row.change_id)
        .map(|change| change.commit_id)
        .collect();
    assert_eq!(commits.len(), 1, "one visible commit of W's change");
    for i in 0..20 {
        assert!(
            file(client, &repo, &commits[0], &format!("api/{i}.txt"))
                .await
                .is_some(),
            "api/{i}.txt"
        );
    }
    assert_eq!(
        file(client, &repo, &commits[0], "busy.txt").await,
        Some(format!("{last}\n")),
        "the last write"
    );
    assert!(!row.divergent, "{row:?}");
    assert_eq!(row.attention(), v1::Attention::Unspecified, "{row:?}");
    assert_eq!(divergent_changes(client, &repo).await, Vec::<String>::new());
    daemon.stop().await;
}

// 16
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines, reason = "both kinds, tagged, then undone")]
async fn edits_are_tagged_and_undoable() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "a.txt", "a\n");
    snapshot(client, &repo, &w).await;

    let rewrote = edit(
        client,
        rewrite(&repo, Some(&w), "@", vec![content("a.txt", "edited\n")]),
    )
    .await;
    let made = edit(
        client,
        new_on(&repo, None, "trunk()", vec![content("n.txt", "n\n")]),
    )
    .await;
    let rewrote_op = rewrote.operation.unwrap();
    let made_op = made.operation.unwrap();
    for (operation, workspace, agent) in
        [(&rewrote_op, w.id.as_str(), "w@test"), (&made_op, "", "")]
    {
        assert_eq!(operation.attributes["reef.kind"], "edit");
        assert_eq!(
            operation
                .attributes
                .get("reef.workspace")
                .map_or("", String::as_str),
            workspace
        );
        assert_eq!(
            operation
                .attributes
                .get("reef.agent")
                .map_or("", String::as_str),
            agent
        );
        let listed = client
            .get_operation(&repo.id, &operation.id)
            .await
            .unwrap()
            .operation
            .unwrap();
        assert_eq!(listed.kind, operation.kind);
        assert_eq!(listed.workspace_id, operation.workspace_id);
        assert_eq!(listed.agent, operation.agent);
        assert_eq!(listed.effects, operation.effects);
    }
    let edits = operations(
        client,
        &repo,
        OperationFilter {
            kinds: vec![OperationKind::Edit.into()],
            ..OperationFilter::default()
        },
    )
    .await;
    let ids: Vec<&str> = edits.iter().map(|op| op.id.as_str()).collect();
    assert_eq!(ids, [made_op.id.as_str(), rewrote_op.id.as_str()]);

    // Undoing the rewrite, from W, puts W's directory back.
    assert_eq!(read(&w, "a.txt").as_deref(), Some("edited\n"));
    client
        .undo_operation(UndoOperationRequest {
            repo: repo.id.clone(),
            workspace: w.id.clone(),
            operation_id: rewrote_op.id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(read(&w, "a.txt").as_deref(), Some("a\n"));
    // Undoing the new change hides it.
    let made_change = made.change.unwrap();
    client
        .undo_operation(UndoOperationRequest {
            repo: repo.id.clone(),
            workspace: String::new(),
            operation_id: made_op.id.clone(),
        })
        .await
        .unwrap();
    let hidden = client
        .get_change(GetChangeRequest {
            repo: repo.id.clone(),
            revision: made_change.commit_id.clone(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert!(hidden.hidden);
    // An edit a later edit rewrote again is superseded.
    let first = edit(
        client,
        rewrite(&repo, Some(&w), "@", vec![content("s.txt", "1")]),
    )
    .await
    .operation
    .unwrap();
    edit(
        client,
        rewrite(&repo, Some(&w), "@", vec![content("s.txt", "2")]),
    )
    .await;
    let error = client
        .undo_operation(UndoOperationRequest {
            repo: repo.id.clone(),
            workspace: w.id.clone(),
            operation_id: first.id,
        })
        .await
        .unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "OPERATION_SUPERSEDED".to_owned())
    );
    daemon.stop().await;
}

// Issue #80, 6: edits W makes while an edit of its `@` is between its write
// and its follow-up are carried onto the edit's commit, in W's change, as
// one snapshot operation.
#[tokio::test(flavor = "multi_thread")]
#[allow(clippy::too_many_lines, reason = "the carry, its operation, then undo")]
async fn edit_carries_late_edits() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "a.txt", "a\n");
    write(&w, "b.txt", "b\n");
    snapshot(client, &repo, &w).await;

    let request = rewrite(
        &repo,
        Some(&w),
        "@",
        vec![content("a.txt", "api a\n"), content("api.txt", "api\n")],
    );
    let rpc = {
        let client = client.clone();
        async move { client.edit_files(request).await.unwrap() }
    };
    let edited = at_follow_hold(&daemon, rpc, || {
        write(&w, "late.txt", "late\n");
        write(&w, "b.txt", "late b\n");
    })
    .await;

    let row = get(client, &repo, &w).await;
    assert_eq!(
        change_commits(client, &repo, &row.change_id).await,
        std::slice::from_ref(&row.commit_id)
    );
    for (path, contents) in [
        ("a.txt", "api a\n"),
        ("api.txt", "api\n"),
        ("late.txt", "late\n"),
        ("b.txt", "late b\n"),
    ] {
        assert_eq!(read(&w, path).as_deref(), Some(contents), "{path} on disk");
        assert_eq!(
            file(client, &repo, &row.commit_id, path).await.as_deref(),
            Some(contents),
            "{path} in the commit"
        );
    }
    assert!(!row.divergent, "{row:?}");
    assert_eq!(row.attention(), v1::Attention::Unspecified, "{row:?}");
    let status = client
        .get_status(v1::GetStatusRequest {
            workspace: w.id.clone(),
            skip_snapshot: true,
            ..v1::GetStatusRequest::default()
        })
        .await
        .unwrap();
    assert!(
        status.workspace_status.unwrap().conflicted_paths.is_empty(),
        "no conflict"
    );

    // One snapshot operation for W after the edit's: the carry.
    let edit_op = edited.operation.unwrap();
    let after = operations_after(client, &repo, &w, &edit_op.id).await;
    assert_eq!(after.len(), 1, "{after:?}");
    let carried = &after[0];
    assert_eq!(carried.kind(), OperationKind::Snapshot);
    assert_eq!(carried.snapshot_kind(), SnapshotKind::Explicit);
    assert_eq!(carried.agent, "w@test");
    let effects = client
        .get_operation(&repo.id, &carried.id)
        .await
        .unwrap()
        .operation
        .unwrap()
        .effects;
    assert!(
        effects
            .iter()
            .flat_map(|effects| &effects.changes)
            .any(|effect| effect.kind() == ChangeEffectKind::Rewritten
                && effect.change_id == row.change_id
                && effect.commit_id == row.commit_id),
        "{effects:?}"
    );
    assert_ne!(
        edited.change.unwrap().commit_id,
        row.commit_id,
        "the carry rewrote the edit's commit"
    );

    // Undo goes newest first: the edit is superseded until the carry is
    // undone.
    let undo = |operation_id: &str| UndoOperationRequest {
        repo: repo.id.clone(),
        workspace: w.id.clone(),
        operation_id: operation_id.to_owned(),
    };
    let error = client.undo_operation(undo(&edit_op.id)).await.unwrap_err();
    assert_eq!(
        reason(&error),
        (Code::FailedPrecondition, "OPERATION_SUPERSEDED".to_owned())
    );
    client.undo_operation(undo(&carried.id)).await.unwrap();
    client.undo_operation(undo(&edit_op.id)).await.unwrap();
    assert_eq!(read(&w, "a.txt").as_deref(), Some("a\n"));
    assert_eq!(read(&w, "b.txt").as_deref(), Some("b\n"));
    assert_eq!(read(&w, "api.txt"), None);
    assert_eq!(read(&w, "late.txt"), None);
    daemon.stop().await;
}

// Issue #80, 7: where the edit and W's late edit change the same line, W's
// one commit has a conflict, with markers on disk.
#[tokio::test(flavor = "multi_thread")]
async fn edit_late_overlap_is_a_conflict() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "a.txt", "one\n");
    snapshot(client, &repo, &w).await;

    let request = rewrite(&repo, Some(&w), "@", vec![content("a.txt", "two\n")]);
    let rpc = {
        let client = client.clone();
        async move { client.edit_files(request).await.unwrap() }
    };
    at_follow_hold(&daemon, rpc, || write(&w, "a.txt", "three\n")).await;

    let row = get(client, &repo, &w).await;
    let commits = change_commits(client, &repo, &row.change_id).await;
    assert_eq!(commits, std::slice::from_ref(&row.commit_id));
    let change = client
        .get_change(GetChangeRequest {
            repo: repo.id.clone(),
            revision: row.commit_id.clone(),
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert!(change.conflict, "{change:?}");
    let status = client
        .get_status(v1::GetStatusRequest {
            workspace: w.id.clone(),
            skip_snapshot: true,
            ..v1::GetStatusRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(status.workspace_status.unwrap().conflicted_paths, ["a.txt"]);
    let on_disk = read(&w, "a.txt").unwrap();
    assert!(
        on_disk.contains("two") && on_disk.contains("three"),
        "{on_disk}"
    );
    assert!(!row.divergent, "{row:?}");
    daemon.stop().await;
}

// Issue #80, 9: after a follow-up that fails, the workspace's next
// snapshot carries.
#[tokio::test(flavor = "multi_thread")]
async fn failed_follow_carries_at_the_next_snapshot() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let scheduler = daemon.handle.state().manager.scheduler();
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "a.txt", "a\n");
    snapshot(client, &repo, &w).await;
    let before = get(client, &repo, &w).await;

    let request = rewrite(&repo, Some(&w), "@", vec![content("api.txt", "api\n")]);
    let rpc = {
        let client = client.clone();
        async move { client.edit_files(request).await.unwrap() }
    };
    let edited = at_follow_hold(&daemon, rpc, || {
        write(&w, "late.txt", "late\n");
        scheduler.fail_scans(true);
    })
    .await;
    let row = get(client, &repo, &w).await;
    assert_eq!(row.commit_id, before.commit_id, "the follow-up failed");
    assert!(
        client
            .snapshot_workspace(&repo.id, &w.id, "")
            .await
            .is_err()
    );

    scheduler.fail_scans(false);
    snapshot(client, &repo, &w).await;
    let row = get(client, &repo, &w).await;
    let after = operations_after(client, &repo, &w, &edited.operation.unwrap().id).await;
    assert_eq!(after.len(), 1, "{after:?}");
    assert_eq!(after[0].kind(), OperationKind::Snapshot);
    assert_eq!(
        change_commits(client, &repo, &row.change_id).await,
        std::slice::from_ref(&row.commit_id)
    );
    for path in ["api.txt", "late.txt"] {
        assert!(
            file(client, &repo, &row.commit_id, path).await.is_some(),
            "{path}"
        );
        assert!(read(&w, path).is_some(), "{path} on disk");
    }
    assert!(!row.divergent, "{row:?}");
    daemon.stop().await;
}

// Issue #80 (review): a carried follow whose check-out fails after its
// commit is merged again over the tree it scanned; the next snapshot
// checks the commit out and records nothing.
#[tokio::test(flavor = "multi_thread")]
async fn failed_check_out_merges_again() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let scheduler = daemon.handle.state().manager.scheduler();
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "a.txt", "a\n");
    snapshot(client, &repo, &w).await;
    let before = get(client, &repo, &w).await;

    let request = rewrite(&repo, Some(&w), "@", vec![content("api.txt", "api\n")]);
    let rpc = {
        let client = client.clone();
        async move { client.edit_files(request).await.unwrap() }
    };
    let edited = at_follow_hold(&daemon, rpc, || {
        write(&w, "late.txt", "late\n");
        scheduler.fail_check_outs(true);
    })
    .await;
    scheduler.fail_check_outs(false);
    let row = get(client, &repo, &w).await;
    assert_eq!(
        row.commit_id, before.commit_id,
        "the row waits for the files"
    );
    assert_eq!(read(&w, "api.txt"), None, "nothing was checked out");

    snapshot(client, &repo, &w).await;
    let row = get(client, &repo, &w).await;
    assert_eq!(
        change_commits(client, &repo, &row.change_id).await,
        std::slice::from_ref(&row.commit_id)
    );
    assert_eq!(read(&w, "api.txt").as_deref(), Some("api\n"));
    assert_eq!(read(&w, "late.txt").as_deref(), Some("late\n"));
    // Only the carried commit's operation follows the edit's.
    let after = operations_after(client, &repo, &w, &edited.operation.unwrap().id).await;
    assert_eq!(after.len(), 1, "{after:?}");
    assert!(!row.divergent, "{row:?}");
    daemon.stop().await;
}

// Issue #80 (review): an edit of X's `@` while X has unsnapshotted edits
// and Y, on a child, writes: step 3's snapshot of X rebases Y's commit
// without moving Y's row, and Y still carries.
#[tokio::test(flavor = "multi_thread")]
async fn descendant_of_a_busy_workspace_carries() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let x = create(client, &repo, "x@test", None).await;
    write(&x, "x.txt", "x\n");
    snapshot(client, &repo, &x).await;
    let x_row = get(client, &repo, &x).await;
    let y = create(
        client,
        &repo,
        "y@test",
        Some(create_workspace_request::Start::NewOn(
            x_row.commit_id.clone(),
        )),
    )
    .await;
    write(&y, "y.txt", "y\n");
    snapshot(client, &repo, &y).await;
    // X's edit, not yet snapshotted: step 3 records it.
    write(&x, "x-pending.txt", "pending\n");

    let request = rewrite(&repo, Some(&x), "@", vec![content("api.txt", "api\n")]);
    let rpc = {
        let client = client.clone();
        async move { client.edit_files(request).await.unwrap() }
    };
    at_follow_hold(&daemon, rpc, || write(&y, "y-late.txt", "late\n")).await;

    let y_row = get(client, &repo, &y).await;
    assert_eq!(y_row.change_id, y.change_id);
    assert_eq!(
        change_commits(client, &repo, &y_row.change_id).await,
        std::slice::from_ref(&y_row.commit_id)
    );
    for path in ["x.txt", "x-pending.txt", "api.txt", "y.txt", "y-late.txt"] {
        assert!(read(&y, path).is_some(), "{path} on disk");
        assert!(
            file(client, &repo, &y_row.commit_id, path).await.is_some(),
            "{path} in Y's commit"
        );
    }
    assert!(!y_row.divergent, "{y_row:?}");
    assert_eq!(divergent_changes(client, &repo).await, Vec::<String>::new());
    daemon.stop().await;
}

/// Snapshots `ws` held between its scan and its commit
/// (`Scheduler::hold_commits`) while `meanwhile` runs; the snapshot's
/// result.
async fn held_at_commit(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    ws: &v1::Workspace,
    meanwhile: impl Future<Output = ()>,
) -> Result<(), ClientError> {
    let scheduler = daemon.handle.state().manager.scheduler();
    let hold = scheduler.hold_commits(ws.id.parse().unwrap()).await;
    let task = {
        let (client, repo, id) = (daemon.client.clone(), repo.id.clone(), ws.id.clone());
        tokio::spawn(async move { client.snapshot_workspace(&repo, &id, "").await.map(drop) })
    };
    let begun = Instant::now();
    while scheduler.held_commits() == 0 {
        assert!(
            begun.elapsed() < Duration::from_secs(30),
            "the snapshot never reached its commit"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    meanwhile.await;
    drop(hold);
    task.await.unwrap()
}

/// Y's one visible commit, not divergent, with `paths` on disk and in it.
async fn y_current(client: &Client, repo: &SyntheticRepo, y: &v1::Workspace, paths: &[&str]) {
    let y_row = get(client, repo, y).await;
    assert_eq!(y_row.change_id, y.change_id);
    assert!(!y_row.divergent, "{y_row:?}");
    assert_eq!(
        change_commits(client, repo, &y.change_id).await,
        std::slice::from_ref(&y_row.commit_id)
    );
    for path in paths {
        assert!(read(y, path).is_some(), "{path} on disk");
        assert!(
            file(client, repo, &y_row.commit_id, path).await.is_some(),
            "{path} in Y's commit"
        );
    }
    assert_eq!(divergent_changes(client, repo).await, Vec::<String>::new());
}

// Issue #91 (second review, 1a): two ancestor snapshots before the busy
// descendant's follow are carried as one move.
#[tokio::test(flavor = "multi_thread")]
async fn two_ancestor_snapshots_before_the_follow_carry_once() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (x, y) = stacked(client, &repo).await;
    // Y's clean snapshot waits after its scan, so the follow X's snapshots
    // queue runs only after both, and Y's edit is not in that scan.
    held_at_commit(&daemon, &repo, &y, async {
        write(&y, "y-late.txt", "late\n");
        write(&x, "x2.txt", "x2\n");
        snapshot(client, &repo, &x).await;
        write(&x, "x3.txt", "x3\n");
        snapshot(client, &repo, &x).await;
    })
    .await
    .unwrap();
    eventually("Y's directory has X's second file", || {
        read(&y, "x3.txt").is_some()
    })
    .await;
    snapshot(client, &repo, &y).await;
    y_current(
        client,
        &repo,
        &y,
        &["x.txt", "x2.txt", "x3.txt", "y.txt", "y-late.txt"],
    )
    .await;
    daemon.stop().await;
}

// Issue #91 (second review, 1b): an ancestor's snapshot that rebases the
// busy descendant's commit between the descendant's scan and its commit
// is merged into that commit, in the same change.
#[tokio::test(flavor = "multi_thread")]
async fn ancestor_snapshot_between_scan_and_commit_merges() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (x, y) = stacked(client, &repo).await;
    write(&y, "y-late.txt", "late\n");
    held_at_commit(&daemon, &repo, &y, async {
        write(&x, "x2.txt", "x2\n");
        snapshot(client, &repo, &x).await;
    })
    .await
    .unwrap();
    y_current(
        client,
        &repo,
        &y,
        &["x.txt", "x2.txt", "y.txt", "y-late.txt"],
    )
    .await;
    assert_eq!(read(&y, "x2.txt").as_deref(), Some("x2\n"));
    daemon.stop().await;
}

// Second review, 1b: where the scan and the rebase change the same lines,
// the change gets jj's conflict, with markers on disk, and does not
// diverge.
#[tokio::test(flavor = "multi_thread")]
async fn ancestor_snapshot_between_scan_and_commit_conflicts_in_the_change() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (x, y) = stacked(client, &repo).await;
    write(&y, "x.txt", "from y\n");
    held_at_commit(&daemon, &repo, &y, async {
        write(&x, "x.txt", "from x\n");
        snapshot(client, &repo, &x).await;
    })
    .await
    .unwrap();
    y_current(client, &repo, &y, &["x.txt", "y.txt"]).await;
    let on_disk = read(&y, "x.txt").unwrap();
    assert!(
        on_disk.contains("<<<<<<<") && on_disk.contains("from y") && on_disk.contains("from x"),
        "{on_disk}"
    );
    daemon.stop().await;
}

// Second review, 1b: a file the rebase changed, edited in the descendant
// after its scan, is not overwritten: the snapshot fails before its
// check-out, and the next one merges the edit in.
#[tokio::test(flavor = "multi_thread")]
async fn ancestor_snapshot_between_scan_and_commit_keeps_an_edit_after_the_scan() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (x, y) = stacked(client, &repo).await;
    write(&y, "y-late.txt", "late\n");
    let held = held_at_commit(&daemon, &repo, &y, async {
        write(&x, "x.txt", "from x\n");
        snapshot(client, &repo, &x).await;
        write(&y, "x.txt", "from y after the scan\n");
    })
    .await;
    assert!(
        held.is_err(),
        "the check-out would overwrite an edit: {held:?}"
    );
    snapshot(client, &repo, &y).await;
    y_current(client, &repo, &y, &["x.txt", "y.txt", "y-late.txt"]).await;
    let on_disk = read(&y, "x.txt").unwrap();
    assert!(on_disk.contains("from y after the scan"), "{on_disk}");
    assert!(on_disk.contains("from x"), "{on_disk}");
    daemon.stop().await;
}

/// Waits up to thirty seconds for `ready`.
async fn until(what: &str, mut ready: impl FnMut() -> bool) {
    let begun = Instant::now();
    while !ready() {
        assert!(
            begun.elapsed() < Duration::from_secs(30),
            "timed out: {what}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// Second R3.11 review, F1: a watcher's narrowed snapshot merged onto an
// ancestor's rebase of its commit does not overwrite a file the agent
// wrote again after the scan.
#[tokio::test(flavor = "multi_thread")]
async fn a_narrowed_snapshot_merged_at_commit_keeps_a_later_edit() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        let watcher = &mut config.file.watcher;
        watcher.backend = reef_daemon::config::WatcherBackend::Notify;
        watcher.debounce = Duration::from_millis(100);
        watcher.min_interval = Duration::from_millis(100);
        watcher.fallback_interval = Duration::from_hours(1);
        watcher.watched_fallback_interval = Duration::from_hours(1);
        watcher.notify_attach_background = false;
    })
    .await;
    let client = &daemon.client;
    let (x, y) = stacked(client, &repo).await;
    let outcomes: Arc<Mutex<Vec<SnapshotOutcome>>> = Arc::default();
    let scheduler = daemon.handle.state().manager.scheduler();
    {
        let sink = Arc::clone(&outcomes);
        let id: reef_daemon::model::WorkspaceId = y.id.parse().unwrap();
        scheduler.on_snapshot(Box::new(move |outcome| {
            if outcome.workspace == id {
                sink.lock().unwrap().push(outcome.clone());
            }
        }));
    }
    let narrowed = || {
        outcomes
            .lock()
            .unwrap()
            .iter()
            .filter(|outcome| outcome.narrowed && outcome.changed)
            .count()
    };
    write(&y, "warm.txt", "w\n");
    until("a narrowed snapshot of Y", || narrowed() == 1).await;

    let hold = scheduler.hold_commits(y.id.parse().unwrap()).await;
    write(&y, "y.txt", "scanned\n");
    until("Y's watcher snapshot waits at its commit", || {
        scheduler.held_commits() == 1
    })
    .await;
    write(&x, "x2.txt", "x2\n");
    snapshot(client, &repo, &x).await;
    write(&y, "y.txt", "after the scan\n");
    drop(hold);

    eventually("Y's directory has X's new file", || {
        read(&y, "x2.txt").is_some()
    })
    .await;
    snapshot(client, &repo, &y).await;
    assert_eq!(read(&y, "y.txt").as_deref(), Some("after the scan\n"));
    let y_row = get(client, &repo, &y).await;
    assert_eq!(
        file(client, &repo, &y_row.commit_id, "y.txt")
            .await
            .as_deref(),
        Some("after the scan\n")
    );
    y_current(client, &repo, &y, &["x2.txt", "warm.txt"]).await;
    daemon.stop().await;
}

// Second R3.11 review, F2: a snapshot that loaded the head before an
// ancestor's commit, and found nothing changed there, does not stop the
// follow from carrying the edits made since.
#[tokio::test(flavor = "multi_thread")]
async fn an_unchanged_snapshot_gives_back_the_carry_it_took() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let scheduler = daemon.handle.state().manager.scheduler();
    let (x, y) = stacked(client, &repo).await;
    // X's snapshot waits after its scan.
    let x_hold = scheduler.hold_commits(x.id.parse().unwrap()).await;
    write(&x, "x2.txt", "x2\n");
    let x_task = {
        let (client, repo, id) = (client.clone(), repo.id.clone(), x.id.clone());
        tokio::spawn(async move { client.snapshot_workspace(&repo, &id, "").await.map(drop) })
    };
    until("X waits at its commit", || scheduler.held_commits() == 1).await;
    // Y's snapshot loads the head, then waits before its scan.
    let scans = scheduler.hold_scans().await;
    let y_task = {
        let (client, repo, id) = (client.clone(), repo.id.clone(), y.id.clone());
        tokio::spawn(async move { client.snapshot_workspace(&repo, &id, "").await.map(drop) })
    };
    until("Y waits before its scan", || scheduler.held_scans() == 1).await;
    // X commits, moving Y and queueing its follow.
    drop(x_hold);
    x_task.await.unwrap().unwrap();
    // Y's snapshot finds nothing changed; the agent
    // writes before the follow scans.
    let y_hold = scheduler.hold_commits(y.id.parse().unwrap()).await;
    drop(scans);
    until("Y waits at its commit", || scheduler.held_commits() == 1).await;
    write(&y, "y-late.txt", "late\n");
    drop(y_hold);
    y_task.await.unwrap().unwrap();

    eventually("Y's directory has X's new file", || {
        read(&y, "x2.txt").is_some()
    })
    .await;
    snapshot(client, &repo, &y).await;
    y_current(client, &repo, &y, &["x2.txt", "y.txt", "y-late.txt"]).await;
    daemon.stop().await;
}

// Third R3.11 review, R3-1: a follow that loaded the head between two
// ancestor commits carries onto the second at its commit.
#[tokio::test(flavor = "multi_thread")]
async fn a_follow_that_loaded_the_head_between_two_ancestor_commits_carries_both() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let scheduler = daemon.handle.state().manager.scheduler();
    let (x, y) = stacked(client, &repo).await;
    // Y's clean snapshot waits at its commit, holding Y's scan lock.
    let y_hold = scheduler.hold_commits(y.id.parse().unwrap()).await;
    let y_task = {
        let (client, repo, id) = (client.clone(), repo.id.clone(), y.id.clone());
        tokio::spawn(async move { client.snapshot_workspace(&repo, &id, "").await.map(drop) })
    };
    until("Y's snapshot waits at its commit", || {
        scheduler.held_commits() == 1
    })
    .await;
    // X's first commit moves Y and queues its follow, which waits
    // for Y's scan lock.
    write(&x, "x2.txt", "x2\n");
    snapshot(client, &repo, &x).await;
    // X's second snapshot waits at its commit.
    let x_hold = scheduler.hold_commits(x.id.parse().unwrap()).await;
    write(&x, "x3.txt", "x3\n");
    let x_task = {
        let (client, repo, id) = (client.clone(), repo.id.clone(), x.id.clone());
        tokio::spawn(async move { client.snapshot_workspace(&repo, &id, "").await.map(drop) })
    };
    until("X's second snapshot waits at its commit", || {
        scheduler.held_commits() == 2
    })
    .await;
    // Y's follow loads the head X's first commit made, then waits before
    // its scan.
    let scans = scheduler.hold_scans().await;
    drop(y_hold);
    y_task.await.unwrap().unwrap();
    until("Y's follow waits before its scan", || {
        scheduler.held_scans() >= 1
    })
    .await;
    // X's second commit moves Y again.
    drop(x_hold);
    x_task.await.unwrap().unwrap();
    write(&y, "y-late.txt", "late\n");
    drop(scans);

    eventually("Y's directory has X's second file", || {
        read(&y, "x3.txt").is_some()
    })
    .await;
    snapshot(client, &repo, &y).await;
    y_current(
        client,
        &repo,
        &y,
        &["x.txt", "x2.txt", "x3.txt", "y.txt", "y-late.txt"],
    )
    .await;
    assert_eq!(change_commits(client, &repo, &x.change_id).await.len(), 1);
    daemon.stop().await;
}

// Fifth R3.11 review, R5-1: an ancestor's snapshot that rebases a busy
// descendant's commit to one with the same tree (X adds Y's own file with
// Y's bytes) still moves it, and Y's snapshot, planned on the old commit,
// lands on the new one.
#[tokio::test(flavor = "multi_thread")]
async fn a_rebase_that_keeps_the_tree_between_scan_and_commit_carries() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (x, y) = stacked(client, &repo).await;
    write(&y, "y-late.txt", "late\n");
    held_at_commit(&daemon, &repo, &y, async {
        write(&x, "y.txt", "y\n");
        snapshot(client, &repo, &x).await;
    })
    .await
    .unwrap();
    y_current(client, &repo, &y, &["x.txt", "y.txt", "y-late.txt"]).await;
    daemon.stop().await;
}

// Fifth R3.11 review, R5-1: a describe of X moves Y to a commit with the
// same tree, and Y's row follows; a later snapshot of X still carries Y's
// edits onto its new commit.
#[tokio::test(flavor = "multi_thread")]
async fn a_describe_below_then_an_ancestor_snapshot_carries() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (x, y) = stacked(client, &repo).await;
    client
        .describe_change(v1::DescribeChangeRequest {
            workspace: x.id.clone(),
            revision: "@".to_owned(),
            description: "x\n".to_owned(),
            ..v1::DescribeChangeRequest::default()
        })
        .await
        .unwrap();
    write(&y, "y-late.txt", "late\n");
    write(&x, "x2.txt", "x2\n");
    snapshot(client, &repo, &x).await;

    eventually("Y's directory has X's new file", || {
        read(&y, "x2.txt").is_some()
    })
    .await;
    snapshot(client, &repo, &y).await;
    y_current(
        client,
        &repo,
        &y,
        &["x.txt", "x2.txt", "y.txt", "y-late.txt"],
    )
    .await;
    daemon.stop().await;
}

// Fifth R3.11 review, R5-1: a describe pauses Y, X's snapshot then rebases
// Y's commit, and the describe keeps that tree. Y's files are still its
// row's, so the describe's follow checks X's file out and carries Y's edit.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_follows_a_descendant_an_ancestor_moved_while_it_was_paused() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let scheduler = daemon.handle.state().manager.scheduler();
    let (x, y) = stacked(client, &repo).await;
    let x_hold = scheduler.hold_commits(x.id.parse().unwrap()).await;
    write(&x, "x2.txt", "x2\n");
    let x_task = {
        let (client, repo, id) = (client.clone(), repo.id.clone(), x.id.clone());
        tokio::spawn(async move { client.snapshot_workspace(&repo, &id, "").await.map(drop) })
    };
    until("X waits at its commit", || scheduler.held_commits() == 1).await;
    // By change id: a describe of `@` would sync X first, behind its hold.
    let describe = {
        let (client, repo, change) = (client.clone(), repo.id.clone(), x.change_id.clone());
        tokio::spawn(async move {
            client
                .describe_change(v1::DescribeChangeRequest {
                    repo,
                    revision: change,
                    description: "x\n".to_owned(),
                    ..v1::DescribeChangeRequest::default()
                })
                .await
                .map(drop)
        })
    };
    let y_id = y.id.parse().unwrap();
    until("the describe pauses Y", || scheduler.is_paused(y_id)).await;
    write(&y, "y-late.txt", "late\n");
    drop(x_hold);
    x_task.await.unwrap().unwrap();
    describe.await.unwrap().unwrap();

    assert_eq!(read(&y, "x2.txt").as_deref(), Some("x2\n"));
    snapshot(client, &repo, &y).await;
    y_current(
        client,
        &repo,
        &y,
        &["x.txt", "x2.txt", "y.txt", "y-late.txt"],
    )
    .await;
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

/// Waits up to ten seconds for `ready`.
async fn eventually(what: &str, mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// X on the trunk and Y on X's commit, each with one snapshotted file.
async fn stacked(client: &Client, repo: &SyntheticRepo) -> (v1::Workspace, v1::Workspace) {
    let x = create(client, repo, "x@test", None).await;
    write(&x, "x.txt", "x1\n");
    snapshot(client, repo, &x).await;
    let x_row = get(client, repo, &x).await;
    let y = create(
        client,
        repo,
        "y@test",
        Some(create_workspace_request::Start::NewOn(x_row.commit_id)),
    )
    .await;
    write(&y, "y.txt", "y\n");
    snapshot(client, repo, &y).await;
    (x, y)
}

// Issue #202: an idle descendant's directory follows its ancestor's
// snapshot, with no snapshot of its own asked for.
#[tokio::test(flavor = "multi_thread")]
async fn idle_descendant_follows_an_ancestors_snapshot() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (x, y) = stacked(client, &repo).await;
    write(&x, "x.txt", "x1-edited\n");
    write(&x, "x2.txt", "x2\n");
    snapshot(client, &repo, &x).await;

    eventually("Y's directory has X's new files", || {
        read(&y, "x2.txt").as_deref() == Some("x2\n")
            && read(&y, "x.txt").as_deref() == Some("x1-edited\n")
    })
    .await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let y_row = loop {
        let row = get(client, &repo, &y).await;
        if change_commits(client, &repo, &y.change_id).await == [row.commit_id.clone()] {
            break row;
        }
        assert!(Instant::now() < deadline, "Y's row never followed: {row:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        change_commits(client, &repo, &y.change_id).await,
        std::slice::from_ref(&y_row.commit_id)
    );
    assert!(!y_row.divergent, "{y_row:?}");
    assert_eq!(read(&y, "y.txt").as_deref(), Some("y\n"));
    daemon.stop().await;
}

// Issue #91: a descendant with unsnapshotted edits carries them onto the
// commit its ancestor's snapshot rebased, in the same change.
#[tokio::test(flavor = "multi_thread")]
async fn busy_descendant_carries_through_an_ancestors_snapshot() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (x, y) = stacked(client, &repo).await;
    write(&y, "y-late.txt", "late\n");
    write(&x, "x2.txt", "x2\n");
    snapshot(client, &repo, &x).await;

    eventually("Y's directory has X's new file", || {
        read(&y, "x2.txt").is_some()
    })
    .await;
    snapshot(client, &repo, &y).await;
    let y_row = get(client, &repo, &y).await;
    assert_eq!(y_row.change_id, y.change_id);
    assert_eq!(
        change_commits(client, &repo, &y.change_id).await,
        std::slice::from_ref(&y_row.commit_id)
    );
    assert!(!y_row.divergent, "{y_row:?}");
    for path in ["x.txt", "x2.txt", "y.txt", "y-late.txt"] {
        assert!(read(&y, path).is_some(), "{path} on disk");
        assert!(
            file(client, &repo, &y_row.commit_id, path).await.is_some(),
            "{path} in Y's commit"
        );
    }
    assert_eq!(divergent_changes(client, &repo).await, Vec::<String>::new());
    daemon.stop().await;
}

// Sixth R3.11 review, R6-1: a describe of Y, by change id, pauses Y. After
// its write and before its follow, X's snapshot rebases Y's commit. Y
// carries its edit onto that commit and gets X's file.
#[tokio::test(flavor = "multi_thread")]
async fn an_ancestor_commit_between_a_describe_and_its_follow_is_followed() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let scheduler = daemon.handle.state().manager.scheduler();
    let (x, y) = stacked(client, &repo).await;
    let hold = scheduler.hold_follows().await;
    let describe = {
        let (client, repo, change) = (client.clone(), repo.id.clone(), y.change_id.clone());
        tokio::spawn(async move {
            client
                .describe_change(v1::DescribeChangeRequest {
                    repo,
                    revision: change,
                    description: "y\n".to_owned(),
                    ..v1::DescribeChangeRequest::default()
                })
                .await
                .map(drop)
        })
    };
    until("the describe waits at its follow", || {
        scheduler.held_follows() == 1
    })
    .await;
    write(&x, "x2.txt", "x2\n");
    snapshot(client, &repo, &x).await;
    write(&y, "y-late.txt", "late\n");
    drop(hold);
    describe.await.unwrap().unwrap();
    snapshot(client, &repo, &y).await;
    y_current(
        client,
        &repo,
        &y,
        &["x.txt", "x2.txt", "y.txt", "y-late.txt"],
    )
    .await;
    daemon.stop().await;
}

// Sixth R3.11 review, R6-1: the same with an edit of Y's change, which
// changes its tree: the follow's snapshot carries across the edit and X's
// snapshot after it.
#[tokio::test(flavor = "multi_thread")]
async fn an_ancestor_commit_between_an_edit_and_its_follow_is_followed() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let scheduler = daemon.handle.state().manager.scheduler();
    let (x, y) = stacked(client, &repo).await;
    let hold = scheduler.hold_follows().await;
    let editing = {
        let (client, request) = (
            client.clone(),
            rewrite(
                &repo,
                None,
                &y.change_id,
                vec![content("y-edit.txt", "e\n")],
            ),
        );
        tokio::spawn(async move { client.edit_files(request).await.map(drop) })
    };
    until("the edit waits at its follow", || {
        scheduler.held_follows() == 1
    })
    .await;
    write(&x, "x2.txt", "x2\n");
    snapshot(client, &repo, &x).await;
    write(&y, "y-late.txt", "late\n");
    drop(hold);
    editing.await.unwrap().unwrap();
    snapshot(client, &repo, &y).await;
    y_current(
        client,
        &repo,
        &y,
        &["x.txt", "x2.txt", "y.txt", "y-edit.txt", "y-late.txt"],
    )
    .await;
    daemon.stop().await;
}

/// Another jj client on the registered root. It loads the head when made
/// and commits bookmark `name` on its working-copy commit when told, so
/// its operation is concurrent with every operation the daemon wrote in
/// between, and the daemon's next load merges the operation heads.
struct OtherClient {
    go: std::sync::mpsc::Sender<()>,
    done: std::thread::JoinHandle<()>,
}

impl OtherClient {
    fn load(root: &Path, name: &'static str) -> Self {
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
            default_workspace_loader_factory,
        };
        let root = root.to_owned();
        let (go, told) = std::sync::mpsc::channel();
        let (tell, loaded) = std::sync::mpsc::channel();
        let done = std::thread::spawn(move || {
            let workspace = default_workspace_loader_factory()
                .create(&root)
                .unwrap()
                .load(
                    &test_settings(),
                    &default_backend_factories(),
                    &default_working_copy_factories(),
                )
                .unwrap();
            let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
            let mut tx = repo.start_transaction();
            let on = repo.view().wc_commit_ids().values().next().unwrap().clone();
            tx.repo_mut().set_local_bookmark_target(
                jj_lib::ref_name::RefName::new(name),
                jj_lib::op_store::RefTarget::normal(on),
            );
            tell.send(()).unwrap();
            told.recv().unwrap();
            pollster::block_on(tx.commit("another client's bookmark")).unwrap();
        });
        loaded.recv().unwrap();
        Self { go, done }
    }

    fn commit_now(self) {
        self.go.send(()).unwrap();
        self.done.join().unwrap();
    }

    async fn commit(self) {
        tokio::task::spawn_blocking(move || self.commit_now())
            .await
            .unwrap();
    }
}

/// The most operation heads the daemon merged in one operation.
async fn merged_heads(client: &Client, repo: &SyntheticRepo) -> usize {
    operations(client, repo, OperationFilter::default())
        .await
        .iter()
        .map(|operation| operation.parent_ids.len())
        .max()
        .unwrap()
}

/// Waits for `ws`'s row to be on its change's one visible commit, with
/// `path` in its directory; panics if the change diverges first.
async fn follows(client: &Client, repo: &SyntheticRepo, ws: &v1::Workspace, path: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let row = get(client, repo, ws).await;
        let commits = change_commits(client, repo, &ws.change_id).await;
        assert!(
            !row.divergent && commits.len() == 1,
            "the change diverged: {commits:?}, {row:?}"
        );
        if commits == [row.commit_id.clone()] && read(ws, path).is_some() {
            return;
        }
        assert!(Instant::now() < deadline, "never followed: {row:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

// Seventh R3.11 review, F1: another jj client's operation, concurrent with
// X's snapshot, leaves a merge of two operation heads. Y, with an edit of
// its own, still carries it onto the commit X's snapshot rebased.
#[tokio::test(flavor = "multi_thread")]
async fn a_descendant_carries_across_a_merge_with_another_clients_operation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (x, y) = stacked(client, &repo).await;
    write(&y, "y-late.txt", "late\n");
    write(&x, "x2.txt", "x2\n");
    let other = OtherClient::load(&repo.root, "other-client");
    held_at_commit(&daemon, &repo, &x, other.commit())
        .await
        .unwrap();

    follows(client, &repo, &y, "x2.txt").await;
    snapshot(client, &repo, &y).await;
    follows(client, &repo, &y, "x2.txt").await;
    let y_row = get(client, &repo, &y).await;
    for path in ["x.txt", "x2.txt", "y.txt", "y-late.txt"] {
        assert!(read(&y, path).is_some(), "{path} on disk");
        assert!(
            file(client, &repo, &y_row.commit_id, path).await.is_some(),
            "{path} in Y's commit"
        );
    }
    assert_eq!(merged_heads(client, &repo).await, 2);
    daemon.stop().await;
}

// Seventh R3.11 review, F1: issue #80's plain case with another jj client
// beside it. An edit of a workspace's `@` by change id, while its agent
// writes, with the other client's operation concurrent with the edit: one
// commit, as without the other client.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_carries_across_a_merge_with_another_clients_operation() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "one.txt", "one\n");
    snapshot(client, &repo, &w).await;
    let other = OtherClient::load(&repo.root, "other-client");
    let request = rewrite(&repo, None, &w.change_id, vec![content("api.txt", "api\n")]);
    let edit = {
        let client = client.clone();
        async move { client.edit_files(request).await }
    };
    let late = || {
        write(&w, "late.txt", "late\n");
        other.commit_now();
    };
    at_follow_hold(&daemon, edit, late).await.unwrap();

    follows(client, &repo, &w, "api.txt").await;
    snapshot(client, &repo, &w).await;
    follows(client, &repo, &w, "api.txt").await;
    let row = get(client, &repo, &w).await;
    for path in ["one.txt", "api.txt", "late.txt"] {
        assert!(
            file(client, &repo, &row.commit_id, path).await.is_some(),
            "{path} in the commit"
        );
    }
    assert_eq!(merged_heads(client, &repo).await, 2);
    daemon.stop().await;
}

// Eighth R3.11 review, R8-1: two other jj clients commit beside X's
// snapshot, and the daemon's next load merges three operation heads in one
// operation. Y still carries its edit onto the commit X's snapshot rebased.
#[tokio::test(flavor = "multi_thread")]
async fn a_descendant_carries_across_a_merge_of_three_operation_heads() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let (x, y) = stacked(client, &repo).await;
    write(&y, "y-late.txt", "late\n");
    write(&x, "x2.txt", "x2\n");
    let others = [
        OtherClient::load(&repo.root, "one"),
        OtherClient::load(&repo.root, "two"),
    ];
    let commit = async {
        for other in others {
            other.commit().await;
        }
    };
    held_at_commit(&daemon, &repo, &x, commit).await.unwrap();

    follows(client, &repo, &y, "x2.txt").await;
    snapshot(client, &repo, &y).await;
    follows(client, &repo, &y, "x2.txt").await;
    let y_row = get(client, &repo, &y).await;
    for path in ["x.txt", "x2.txt", "y.txt", "y-late.txt"] {
        assert!(read(&y, path).is_some(), "{path} on disk");
        assert!(
            file(client, &repo, &y_row.commit_id, path).await.is_some(),
            "{path} in Y's commit"
        );
    }
    assert_eq!(merged_heads(client, &repo).await, 3);
    daemon.stop().await;
}

// Eighth R3.11 review, R8-1: issue #80's plain case beside two other jj
// clients: one commit, as without them.
#[tokio::test(flavor = "multi_thread")]
async fn a_write_carries_across_a_merge_of_three_operation_heads() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let w = create(client, &repo, "w@test", None).await;
    write(&w, "one.txt", "one\n");
    snapshot(client, &repo, &w).await;
    let others = [
        OtherClient::load(&repo.root, "one"),
        OtherClient::load(&repo.root, "two"),
    ];
    let request = rewrite(&repo, None, &w.change_id, vec![content("api.txt", "api\n")]);
    let edit = {
        let client = client.clone();
        async move { client.edit_files(request).await }
    };
    let late = || {
        write(&w, "late.txt", "late\n");
        others.into_iter().for_each(OtherClient::commit_now);
    };
    at_follow_hold(&daemon, edit, late).await.unwrap();

    follows(client, &repo, &w, "api.txt").await;
    snapshot(client, &repo, &w).await;
    follows(client, &repo, &w, "api.txt").await;
    let row = get(client, &repo, &w).await;
    for path in ["one.txt", "api.txt", "late.txt"] {
        assert!(
            file(client, &repo, &row.commit_id, path).await.is_some(),
            "{path} in the commit"
        );
    }
    assert_eq!(merged_heads(client, &repo).await, 3);
    daemon.stop().await;
}

// ADR 0084: a restart between Reef's move of a workspace and the
// workspace's follow costs nothing. X's snapshot rebases Y's commit, and
// Y's follow fails before it scans; the daemon restarts; Y's next snapshot
// reads the move from the operation log and carries Y's edit.
#[tokio::test(flavor = "multi_thread")]
async fn a_restart_between_a_move_and_its_follow_still_carries() {
    let (daemon, repo) = fixture().await;
    let (x, y) = stacked(&daemon.client, &repo).await;
    let scheduler = daemon.handle.state().manager.scheduler();
    write(&y, "y-late.txt", "late\n");
    write(&x, "x2.txt", "x2\n");
    // X's snapshot has scanned: every scan after it fails, Y's follow too.
    held_at_commit(&daemon, &repo, &x, async { scheduler.fail_scans(true) })
        .await
        .unwrap();
    let y_row = get(&daemon.client, &repo, &y).await;
    assert_ne!(
        change_commits(&daemon.client, &repo, &y.change_id).await,
        std::slice::from_ref(&y_row.commit_id),
        "X's snapshot moved Y's commit"
    );

    let daemon = daemon.restart().await;
    let client = &daemon.client;
    assert_eq!(read(&y, "x2.txt"), None, "Y had not followed");
    snapshot(client, &repo, &y).await;
    y_current(
        client,
        &repo,
        &y,
        &["x.txt", "x2.txt", "y.txt", "y-late.txt"],
    )
    .await;
    daemon.stop().await;
}
