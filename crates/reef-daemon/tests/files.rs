//! `reef.api.v1.FileService` through the daemon (R2.04, tests 9 to 16):
//! reading and listing at a revision with the snapshot rule and pinned
//! pages, and edits without a workspace, with the live workspaces they
//! move, their refusals, concurrency, operations and undo.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, ChangeEffectKind, CreateWorkspaceRequest, EditFilesRequest, EditFilesResponse, EntryType,
    FileChangeKind, FileDelete, FileEdit, GetChangeRequest, GetDiffRequest, ListFilesRequest,
    ListOperationsRequest, OperationFilter, OperationKind, ReadFileRequest, SnapshotKind,
    TextReplace, UndoOperationRequest, create_workspace_request, edit_files_request, file_edit,
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

// 15
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
    let diverged = commits.len() > 1;
    println!(
        "edits while the workspace writes: {} visible commits, last write {last}",
        commits.len()
    );
    for i in 0..20 {
        let mut found = false;
        for commit in &commits {
            found |= file(client, &repo, commit, &format!("api/{i}.txt"))
                .await
                .is_some();
        }
        assert!(found, "api/{i}.txt is in no visible commit of the change");
    }
    let mut last_found = false;
    for commit in &commits {
        last_found |=
            file(client, &repo, commit, "busy.txt").await.as_deref() == Some(&format!("{last}\n"));
    }
    assert!(last_found, "the last write is in a visible commit");
    if diverged {
        assert!(row.divergent, "{row:?}");
    }
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
