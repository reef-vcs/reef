//! Disk accounting and the free-space floor end to end (#272, ADR 0076):
//! the kept figure in `ListWorkspaces`, the dirty walk after build output
//! is written, the floor refusing creates and store builds and nothing
//! else, and the status. Free space is injected through the test hook.
//! Numbered as the brief numbers them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns"
)]

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::{SyntheticRepo, TestDaemon};
use reef_api::v1::{
    self, CreateWorkspaceRequest, DiskLevel, ListWorkspacesRequest, MoveWorkspaceRequest,
    Workspace, move_workspace_request,
};
use reef_daemon::client::ClientError;
use reef_daemon::config::{Config, DiskAmount};
use reef_daemon::error::detail_of;
use reef_lib::error::ErrorKind;
use reef_testing::synth::Preset;

const OWNER: &str = "disk@host";

/// Free space far above any floor.
const PLENTY: u64 = 1 << 43;

/// The default floor (`TestDaemon` turns it off), and no automatic
/// snapshots.
fn floored(config: &mut Config) {
    reef_daemon::testing::no_automatic_snapshots(config);
    config.file.disk.floor = DiskAmount::Auto;
}

fn request(repo: &SyntheticRepo) -> CreateWorkspaceRequest {
    CreateWorkspaceRequest {
        repo: repo.id.clone(),
        owner: OWNER.to_owned(),
        ..CreateWorkspaceRequest::default()
    }
}

async fn create(daemon: &TestDaemon, repo: &SyntheticRepo) -> Workspace {
    daemon
        .client
        .create_workspace(request(repo))
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn listed(daemon: &TestDaemon, id: &str) -> (Workspace, Option<v1::DiskStatus>) {
    let response = daemon
        .client
        .list_workspaces_page(ListWorkspacesRequest::default())
        .await
        .unwrap();
    let workspace = response
        .workspaces
        .into_iter()
        .find(|ws| ws.id == id)
        .expect("listed");
    (workspace, response.disk_status.map(|status| *status))
}

fn write(path: &Path, bytes: usize) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![3_u8; bytes]).unwrap();
}

/// The reason and metadata of a refused call.
fn refusal(error: &ClientError) -> (ErrorKind, v1::ErrorDetail) {
    let ClientError::Rpc { kind, status, .. } = error else {
        panic!("not an RPC error: {error:?}");
    };
    (*kind, detail_of(status).expect("an ErrorDetail"))
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn disk_floor_refuses_creates() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, floored).await;
    let manager = &daemon.handle.state().manager;
    // Plenty free, whatever the machine has: only the test moves it.
    manager.disk().set_free_for_tests(Some(PLENTY));
    let idle = create(&daemon, &repo).await;
    // Its holder is gone: its lease's presence entry ends.
    manager.presence().end_workspace(idle.id.parse().unwrap());
    manager.disk().set_idle_after_for_tests(Duration::ZERO);
    manager.disk().set_free_for_tests(Some(1 << 30));
    let bases = std::fs::read_dir(
        daemon
            .state_dir()
            .join("repos")
            .join(&repo.id)
            .join("bases"),
    )
    .unwrap()
    .count();
    let error = daemon
        .client
        .create_workspace(request(&repo))
        .await
        .unwrap_err();
    let (kind, detail) = refusal(&error);
    assert_eq!(kind, ErrorKind::Conflict);
    assert_eq!(detail.reason, reef_api::error::DISK_SPACE_LOW);
    assert_eq!(detail.metadata["free_bytes"], (1_u64 << 30).to_string());
    assert!(detail.metadata["floor_bytes"].parse::<u64>().unwrap() >= 20 << 30);
    assert_eq!(detail.metadata["largest_idle"], idle.id);
    assert_eq!(detail.next, ["reef", "ws", "release", idle.id.as_str()]);
    assert!(
        detail.hint.contains("nothing written is lost"),
        "{}",
        detail.hint
    );
    // Nothing was made: no new base, no second workspace.
    assert_eq!(
        std::fs::read_dir(
            daemon
                .state_dir()
                .join("repos")
                .join(&repo.id)
                .join("bases")
        )
        .unwrap()
        .count(),
        bases
    );
    let all = daemon
        .client
        .list_workspaces(ListWorkspacesRequest::default())
        .await
        .unwrap();
    assert_eq!(all.len(), 1);
    // Nobody idle: `next` lists the workspaces.
    manager
        .disk()
        .set_idle_after_for_tests(Duration::from_hours(1));
    let error = daemon
        .client
        .create_workspace(request(&repo))
        .await
        .unwrap_err();
    let (_, detail) = refusal(&error);
    assert_eq!(detail.next, ["reef", "ws", "list"]);
    assert!(!detail.metadata.contains_key("largest_idle"));
    // Space back: creates go on.
    manager.disk().set_free_for_tests(Some(PLENTY));
    create(&daemon, &repo).await;
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn disk_floor_never_refuses() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, floored).await;
    let manager = &daemon.handle.state().manager;
    // Plenty free, whatever the machine has: only the test moves it.
    manager.disk().set_free_for_tests(Some(PLENTY));
    let ws = create(&daemon, &repo).await;
    let other = create(&daemon, &repo).await;
    manager.disk().set_free_for_tests(Some(0));
    // A snapshot of an edit.
    write(&Path::new(&ws.path).join("edited.txt"), 100);
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(snapshot.operation.is_some());
    // A move onto the other workspace's change.
    daemon
        .client
        .move_workspace(MoveWorkspaceRequest {
            repo: repo.id.clone(),
            workspace: other.id.clone(),
            target: Some(move_workspace_request::Target::Change(ws.change_id.clone())),
            ..MoveWorkspaceRequest::default()
        })
        .await
        .unwrap();
    // A renewal.
    let lease = ws.lease.as_ref().unwrap();
    daemon
        .client
        .renew_lease(&repo.id, &ws.id, &lease.id)
        .await
        .unwrap();
    // A release, whose final snapshot keeps the edit, and GC.
    write(&Path::new(&ws.path).join("late.txt"), 100);
    let released = daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    assert!(released.snapshot_taken);
    daemon
        .client
        .collect_garbage(&repo.id, false)
        .await
        .unwrap();
    // The level says so.
    let (_, status) = listed(&daemon, &other.id).await;
    assert_eq!(status.unwrap().level(), DiskLevel::Low);
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn disk_figures() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.disk.floor = DiskAmount::Auto;
    })
    .await;
    let manager = &daemon.handle.state().manager;
    // Plenty free, whatever the machine has: only the test moves it.
    manager.disk().set_free_for_tests(Some(PLENTY));
    manager
        .disk()
        .set_interval_for_tests(Duration::from_millis(200));
    let ws = create(&daemon, &repo).await;
    // At create: a zero figure, measured at the create, with no walk.
    let (listed_ws, status) = listed(&daemon, &ws.id).await;
    let disk = listed_ws.disk.unwrap();
    assert_eq!(disk.tracked_bytes + disk.build_bytes, 0, "{disk:?}");
    assert_eq!(disk.measured_at, ws.created_at);
    let status = status.unwrap();
    assert_eq!(status.level(), DiskLevel::Ok, "{status:?}");
    assert_ne!(status.volumes.len(), 0);
    // Build output under an ignored directory, and a tracked edit: the
    // watch events mark it dirty (ignored paths included), and the loop
    // walks it.
    let root = Path::new(&ws.path);
    std::fs::write(root.join(".gitignore"), "target/\n").unwrap();
    daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let walks = manager.disk().walks();
    write(&root.join("target/debug/out.bin"), 3 << 20);
    write(&root.join("target/debug/deps/lib.rlib"), 1 << 20);
    let deadline = Instant::now() + Duration::from_secs(60);
    let figure = loop {
        let (listed_ws, _) = listed(&daemon, &ws.id).await;
        let disk = listed_ws.disk.unwrap();
        if disk.build_bytes >= 4 << 20 {
            break disk;
        }
        assert!(
            Instant::now() < deadline,
            "no walk saw the build output: {disk:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(manager.disk().walks() > walks);
    assert_eq!(figure.largest[0].path, "target", "{figure:?}");
    assert!(figure.largest[0].bytes >= 4 << 20);
    assert!(!figure.incomplete);
    // Nothing written: no further background walk.
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let settled = manager.disk().walks();
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(manager.disk().walks(), settled, "walked a clean workspace");
    // GetWorkspace's walk updates the kept figure.
    write(&root.join("target/more.bin"), 1 << 20);
    let inspected = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap();
    let usage = inspected.disk_usage.unwrap();
    assert!(usage.allocated_bytes >= 5 << 20, "{usage:?}");
    let kept = inspected.workspace.unwrap().disk.unwrap();
    assert!(kept.build_bytes >= 5 << 20, "{kept:?}");
    // The figure survives a restart.
    let id = ws.id.clone();
    let daemon = daemon.restart().await;
    let (listed_ws, _) = listed(&daemon, &id).await;
    assert!(listed_ws.disk.unwrap().build_bytes >= 5 << 20);
    // Released: no figure.
    daemon
        .client
        .release_workspace(&repo.id, &id, false, false)
        .await
        .unwrap();
    let state = daemon.handle.state();
    assert!(
        state
            .db
            .workspace_disk(id.parse().unwrap())
            .unwrap()
            .is_none()
    );
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn disk_status() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, floored).await;
    let manager = &daemon.handle.state().manager;
    // Plenty free, whatever the machine has: only the test moves it.
    manager.disk().set_free_for_tests(Some(PLENTY));
    let ws = create(&daemon, &repo).await;
    let status = daemon.client.status().await.unwrap().disk.unwrap();
    assert_eq!(status.level(), DiskLevel::Ok);
    // One volume holding both, under a test's temporary directory.
    assert_eq!(status.volumes.len(), 1, "{status:?}");
    assert_eq!(status.volumes[0].holds, ["workspaces", "state"]);
    assert!(status.bases_bytes > 0, "{status:?}");
    assert_eq!(
        status.reef_bytes,
        status.workspaces_bytes + status.bases_bytes + status.stores_bytes + status.state_bytes
    );
    assert!(status.largest_idle.is_none());
    // Near the floor: warn, with the purgeable figure read.
    let floor = status.volumes[0].floor_bytes;
    manager.disk().set_free_for_tests(Some(floor + 1));
    let status = daemon.client.status().await.unwrap().disk.unwrap();
    assert_eq!(status.level(), DiskLevel::Warn);
    assert_eq!(
        status.reasons().collect::<Vec<_>>(),
        [v1::DiskReason::FreeNearFloor]
    );
    // Low, with an idle workspace named (once its holder's entry ends).
    manager.disk().set_idle_after_for_tests(Duration::ZERO);
    manager.disk().set_free_for_tests(Some(floor - 1));
    let status = daemon.client.status().await.unwrap().disk.unwrap();
    assert!(status.largest_idle.is_none(), "a fresh lease is not idle");
    manager.presence().end_workspace(ws.id.parse().unwrap());
    manager.disk().set_free_for_tests(Some(floor - 1));
    let status = daemon.client.status().await.unwrap().disk.unwrap();
    assert_eq!(status.level(), DiskLevel::Low);
    assert_eq!(status.largest_idle.unwrap().workspace, ws.id);
}
