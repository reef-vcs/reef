//! Overlaps (#263, ADR 0079): a workspace's status and `GetWorkspace`
//! name the other open changes that change its paths. Workspaces write
//! and snapshot explicitly, except in the first test, which waits for the
//! watcher as an agent would.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::op_store::RefTarget;
use jj_lib::ref_name::RefName;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use reef_api::v1::{
    self, CreateWorkspaceRequest, GetStatusRequest, GetWorkspaceRequest, Workspace,
};
use reef_daemon::client::Client;
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_testing::synth::Preset;

async fn fixture_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        configure(config);
    })
    .await
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
}

async fn create(client: &Client, repo: &str, owner: &str, new_on: Option<&str>) -> Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: owner.to_owned(),
            start: new_on.map(|on| v1::create_workspace_request::Start::NewOn(on.to_owned())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// Writes `text` to `file` in `ws` and snapshots it.
async fn write(client: &Client, ws: &Workspace, file: &str, text: &str) {
    let path = Path::new(&ws.path).join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    client.snapshot_workspace("", &ws.id, "").await.unwrap();
}

/// `ws` as its status reports it, at head.
async fn status(client: &Client, ws: &Workspace) -> Workspace {
    client
        .get_status(GetStatusRequest {
            workspace: ws.id.clone(),
            ..GetStatusRequest::default()
        })
        .await
        .unwrap()
        .workspace_status
        .unwrap()
        .workspace
        .unwrap()
}

async fn inspect(client: &Client, ws: &Workspace) -> Workspace {
    client
        .get_workspace(GetWorkspaceRequest {
            workspace: ws.id.clone(),
            skip_disk_usage: true,
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

fn overlap_workspace_ids(ws: &Workspace) -> Vec<&str> {
    ws.overlaps
        .iter()
        .map(|overlap| overlap.workspace_id.as_str())
        .collect()
}

/// The repository at `root`, loaded at its head with jj-lib.
fn load(root: &Path) -> std::sync::Arc<ReadonlyRepo> {
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
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
    pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap()
}

/// A commit on `parent` writing `files`, outside the daemon, as the jj CLI
/// would; its id. With `main`, the `main` bookmark moves to it.
fn commit_outside(root: &Path, parent: &str, files: &[(&str, &str)], move_main: bool) -> String {
    use jj_lib::backend::{CopyId, TreeValue};
    use jj_lib::repo_path::RepoPathBuf;
    let repo = load(root);
    let mut tx = repo.start_transaction();
    let store = tx.repo().store().clone();
    let parent =
        pollster::block_on(store.get_commit_async(&CommitId::try_from_hex(parent).unwrap()))
            .unwrap();
    let mut builder = jj_lib::merged_tree_builder::MergedTreeBuilder::new(parent.tree());
    for (file, text) in files {
        let path = RepoPathBuf::from_internal_string(*file).unwrap();
        let id = pollster::block_on(store.write_file(&path, &mut text.as_bytes())).unwrap();
        builder.set_or_remove(
            path,
            Some(TreeValue::File {
                id,
                executable: false,
                copy_id: CopyId::placeholder(),
            }),
        );
    }
    let tree = pollster::block_on(builder.write_tree()).unwrap();
    let commit = pollster::block_on(
        tx.repo_mut()
            .new_commit(vec![parent.id().clone()], tree)
            .set_description("outside")
            .write(),
    )
    .unwrap();
    if move_main {
        tx.repo_mut().set_local_bookmark_target(
            RefName::new("main"),
            RefTarget::normal(commit.id().clone()),
        );
    }
    pollster::block_on(tx.commit("outside")).unwrap();
    commit.id().hex()
}

/// The commit `ws`'s change is on: its first parent.
async fn base(client: &Client, ws: &Workspace) -> String {
    let status = client
        .get_status(GetStatusRequest {
            workspace: ws.id.clone(),
            skip_snapshot: true,
            ..GetStatusRequest::default()
        })
        .await
        .unwrap();
    status.workspace_status.unwrap().parents[0]
        .commit_id
        .clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn each_names_the_other_within_one_snapshot_interval() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.history.enabled = false;
        config.file.watcher.notify_attach_background = false;
    })
    .await;
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a", None).await;
    let b = create(client, &repo.id, "agent-b", None).await;
    std::fs::write(Path::new(&a.path).join("shared.txt"), "a\n").unwrap();
    std::fs::write(Path::new(&b.path).join("shared.txt"), "b\n").unwrap();
    // One snapshot interval: the debounce, the minimum interval, and a
    // scan, with slack for a loaded machine.
    let watcher = &daemon.config.file.watcher;
    let interval = watcher.debounce + watcher.min_interval + Duration::from_secs(5);
    for (me, other) in [(&a, &b), (&b, &a)] {
        let begun = Instant::now();
        let found = loop {
            let found = status(client, me).await;
            if !found.overlaps.is_empty() || begun.elapsed() > interval {
                break found;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        };
        assert_eq!(
            overlap_workspace_ids(&found),
            [other.id.as_str()],
            "{found:?}"
        );
        let overlap = &found.overlaps[0];
        assert_eq!(overlap.owner, other.owner);
        assert!(overlap.active);
        assert_eq!(overlap.paths, ["shared.txt"]);
        assert_eq!(overlap.path_count, 1);
        assert_eq!(overlap.change_id, other.change_id);
        assert_eq!(found.overlap_count, 1);
    }
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn different_files_do_not_overlap() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a", None).await;
    let b = create(client, &repo.id, "agent-b", None).await;
    write(client, &a, "a.txt", "a\n").await;
    write(client, &b, "b.txt", "b\n").await;
    let found = status(client, &a).await;
    assert!(found.overlaps.is_empty(), "{found:?}");
    assert_eq!(found.overlap_count, 0);
    assert_eq!(inspect(client, &a).await.overlaps, []);
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_overlap_ends_when_the_other_reverts() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a", None).await;
    let b = create(client, &repo.id, "agent-b", None).await;
    write(client, &a, "dir/shared.txt", "a\n").await;
    write(client, &b, "dir/shared.txt", "b\n").await;
    assert_eq!(
        overlap_workspace_ids(&status(client, &a).await),
        [b.id.as_str()]
    );
    std::fs::remove_file(Path::new(&b.path).join("dir/shared.txt")).unwrap();
    client.snapshot_workspace("", &b.id, "").await.unwrap();
    let found = status(client, &a).await;
    assert!(found.overlaps.is_empty(), "{found:?}");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_released_workspaces_change_counts_within_max_age() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a", None).await;
    let b = create(client, &repo.id, "agent-b", None).await;
    write(client, &a, "shared.txt", "a\n").await;
    write(client, &b, "shared.txt", "b\n").await;
    client
        .release_workspace("", &b.id, true, false)
        .await
        .unwrap();
    let found = status(client, &a).await;
    assert_eq!(overlap_workspace_ids(&found), [b.id.as_str()], "{found:?}");
    assert!(!found.overlaps[0].active);
    assert_eq!(found.overlaps[0].owner, "agent-b");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn max_age_zero_counts_only_active_workspaces() {
    let (daemon, repo) = fixture_with(|config| config.file.overlaps.max_age = Duration::ZERO).await;
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a", None).await;
    let b = create(client, &repo.id, "agent-b", None).await;
    let c = create(client, &repo.id, "agent-c", None).await;
    write(client, &a, "shared.txt", "a\n").await;
    write(client, &b, "shared.txt", "b\n").await;
    write(client, &c, "shared.txt", "c\n").await;
    client
        .release_workspace("", &b.id, true, false)
        .await
        .unwrap();
    assert_eq!(
        overlap_workspace_ids(&status(client, &a).await),
        [c.id.as_str()]
    );
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stacked_work_does_not_overlap() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a", None).await;
    write(client, &a, "shared.txt", "a\n").await;
    // On a's change: its descendant.
    let stacked = create(client, &repo.id, "agent-b", Some(&a.change_id)).await;
    write(client, &stacked, "shared.txt", "b\n").await;
    for ws in [&a, &stacked] {
        let found = status(client, ws).await;
        assert_eq!(found.overlaps, [], "{}", ws.owner);
    }
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_shared_change_gone_divergent_does_not_overlap_itself() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a", None).await;
    let same = client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "agent-c".to_owned(),
            start: Some(v1::create_workspace_request::Start::Change(
                a.change_id.clone(),
            )),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    // Both write one file: two commits of one change, neither the other's
    // ancestor.
    write(client, &a, "shared.txt", "a\n").await;
    write(client, &same, "shared.txt", "c\n").await;
    let found = status(client, &same).await;
    assert!(found.divergent, "{found:?}");
    for ws in [&a, &same] {
        let found = status(client, ws).await;
        assert_eq!(found.overlaps, [], "{}", ws.owner);
    }
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_path_landed_in_substance_does_not_count() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a", None).await;
    let on = base(client, &a).await;
    // Trunk gets `x`; one open change says the same, another does not.
    commit_outside(&repo.root, &on, &[("shared.txt", "x\n")], true);
    let same = commit_outside(&repo.root, &on, &[("shared.txt", "x\n")], false);
    let differs = commit_outside(&repo.root, &on, &[("shared.txt", "y\n")], false);
    write(client, &a, "shared.txt", "a\n").await;
    let found = status(client, &a).await;
    let commits: Vec<&str> = found
        .overlaps
        .iter()
        .map(|overlap| overlap.commit_id.as_str())
        .collect();
    assert_eq!(commits, [differs.as_str()], "{found:?}");
    assert!(!commits.contains(&same.as_str()));
    // No workspace has been on it: named by its author.
    assert_eq!(found.overlaps[0].workspace_id, "");
    assert_eq!(found.overlaps[0].owner, "reef-test@example.com");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ten_changes_and_ten_paths_are_listed_and_the_rest_counted() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a", None).await;
    let on = base(client, &a).await;
    let files: Vec<(String, String)> = (0..15)
        .map(|n| (format!("many/f{n:02}.txt"), format!("{n}\n")))
        .collect();
    let borrowed: Vec<(&str, &str)> = files
        .iter()
        .map(|(path, text)| (path.as_str(), text.as_str()))
        .collect();
    for _ in 0..12 {
        commit_outside(&repo.root, &on, &borrowed, false);
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    for (path, _) in &files {
        std::fs::create_dir_all(Path::new(&a.path).join("many")).unwrap();
        std::fs::write(Path::new(&a.path).join(path), "a\n").unwrap();
    }
    client.snapshot_workspace("", &a.id, "").await.unwrap();
    let found = status(client, &a).await;
    assert_eq!(found.overlaps.len(), 10);
    assert_eq!(found.overlap_count, 12);
    let overlap = &found.overlaps[0];
    assert_eq!(overlap.paths.len(), 10);
    assert_eq!(overlap.path_count, 15);
    assert_eq!(overlap.paths[0], "many/f00.txt");
    // Newest written first.
    let times: Vec<_> = found
        .overlaps
        .iter()
        .map(|overlap| {
            let at = overlap.written_at.as_ref().unwrap();
            (at.seconds, at.nanos)
        })
        .collect();
    assert!(times.windows(2).all(|pair| pair[0] >= pair[1]), "{times:?}");
    daemon.stop().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_get_workspace_agree_and_an_operation_has_none() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a", None).await;
    let b = create(client, &repo.id, "agent-b", None).await;
    write(client, &a, "shared.txt", "a\n").await;
    write(client, &b, "shared.txt", "b\n").await;
    let by_status = status(client, &a).await;
    let by_inspect = inspect(client, &a).await;
    assert_eq!(by_status.overlaps, by_inspect.overlaps);
    assert_eq!(by_status.overlap_count, by_inspect.overlap_count);
    assert_eq!(overlap_workspace_ids(&by_status), [b.id.as_str()]);
    let head = client
        .get_status(GetStatusRequest {
            workspace: a.id.clone(),
            ..GetStatusRequest::default()
        })
        .await
        .unwrap()
        .at_operation;
    let at = client
        .get_status(GetStatusRequest {
            workspace: a.id.clone(),
            at_operation: head,
            ..GetStatusRequest::default()
        })
        .await
        .unwrap()
        .workspace_status
        .unwrap();
    assert_eq!(at.workspace.unwrap().overlaps, []);
    daemon.stop().await;
}
