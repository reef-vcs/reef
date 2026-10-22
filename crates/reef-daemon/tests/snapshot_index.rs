//! The `snapshots` index end to end (R1.16): every snapshot operation the
//! daemon writes gets a local row, and the start-time reindex rebuilds and
//! prunes the table from jj's operation log. Numbered as the brief numbers
//! them; 6 is extra.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use common::{SyntheticRepo, TestDaemon, test_settings};
use reef_daemon::Daemon;
use reef_daemon::client::Client;
use reef_daemon::db::Db;
use reef_daemon::model::{RepoId, Timestamp, WorkspaceId};
use reef_daemon::proto::v0::{
    CreateRequest, ListWorkspacesRequest, MoveRequest, Workspace, create_request, move_request,
};
use reef_daemon::snapshot::SnapshotKind;
use reef_daemon::snapshot::index::{self, ReindexReport, SnapshotEntry};
use reef_lib::jj::Repo;
use reef_lib::jj::oplog::{OpEntry, is_reef_snapshot};
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";

/// A daemon with short watcher intervals and no fallback snapshots during
/// a test, so every operation comes from a step the test takes.
async fn fixture() -> (TestDaemon, SyntheticRepo) {
    // `TEST_LOG=reef_daemon=debug` shows the daemons' logs.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("TEST_LOG"))
        .with_test_writer()
        .try_init();
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        let watcher = &mut config.file.watcher;
        watcher.debounce = Duration::from_millis(100);
        watcher.min_interval = Duration::from_millis(100);
        watcher.fallback_interval = Duration::from_secs(3600);
    })
    .await
}

async fn create(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    start: Option<create_request::Start>,
) -> Workspace {
    daemon
        .client
        .create_workspace(CreateRequest {
            repo: repo.id.clone(),
            start,
            owner: OWNER.to_owned(),
            ..CreateRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

fn id_of(ws: &Workspace) -> WorkspaceId {
    ws.id.parse().unwrap()
}

fn repo_id(repo: &SyntheticRepo) -> RepoId {
    repo.id.parse().unwrap()
}

fn write(ws: &Workspace, path: &str, contents: &str) {
    std::fs::write(Path::new(&ws.path).join(path), contents).unwrap();
}

/// Writes a file and waits for the watcher to snapshot it; returns the
/// operation.
async fn watcher_snapshot(daemon: &TestDaemon, repo: &SyntheticRepo, ws: &Workspace) -> String {
    let before = inspect(daemon, repo, ws).await.last_snapshot_op;
    write(ws, "watched.txt", "seen by the watcher\n");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let op = inspect(daemon, repo, ws).await.last_snapshot_op;
        if !op.is_empty() && op != before {
            return op;
        }
        assert!(Instant::now() < deadline, "no watcher snapshot within 10 s");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Stops watcher snapshots so that from here on only the test's own steps
/// write operations. Dropping new events is not enough: a batch already
/// delivered may have marked a workspace dirty, and its snapshot could take
/// in the next file the test writes. So wait out the debounce and
/// `min_interval`, then snapshot each workspace once to absorb whatever is
/// pending (normally nothing, which writes no operation) and clear its mark.
async fn quiesce(daemon: &TestDaemon, repo: &SyntheticRepo, workspaces: &[&Workspace]) {
    daemon.handle.state().manager.scheduler().pause_watcher();
    tokio::time::sleep(Duration::from_millis(500)).await;
    for ws in workspaces {
        daemon
            .client
            .snapshot_workspace(&repo.id, &ws.id, "")
            .await
            .unwrap();
    }
}

/// Writes a file and takes an explicit snapshot; returns the operation.
async fn explicit_snapshot(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    ws: &Workspace,
    path: &str,
) -> String {
    write(ws, path, &format!("{path}\n"));
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(snapshot.changed, "the write is new");
    snapshot.op_id
}

/// Writes a file and moves the workspace to its own change (R1.06), which
/// snapshots it first; returns that snapshot's operation.
async fn move_snapshot(daemon: &TestDaemon, repo: &SyntheticRepo, ws: &Workspace) -> String {
    write(ws, "moved.txt", "written before a move\n");
    let moved = daemon
        .client
        .move_workspace(MoveRequest {
            repo: repo.id.clone(),
            workspace: ws.id.clone(),
            target: Some(move_request::Target::Change(ws.change_id.clone())),
            ..MoveRequest::default()
        })
        .await
        .unwrap();
    assert!(!moved.final_op_id.is_empty(), "the move's snapshot wrote");
    moved.final_op_id
}

/// Writes a file and releases with a final snapshot; returns the
/// operation.
async fn final_snapshot(daemon: &TestDaemon, repo: &SyntheticRepo, ws: &Workspace) -> String {
    write(ws, "last.txt", "written before release\n");
    let released = daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    assert!(!released.final_op_id.is_empty(), "the final snapshot wrote");
    released.final_op_id
}

async fn inspect(daemon: &TestDaemon, repo: &SyntheticRepo, ws: &Workspace) -> Workspace {
    daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// Every row of `ws`, newest first.
fn rows(db: &Db, ws: &Workspace) -> Vec<SnapshotEntry> {
    index::recent(db, id_of(ws), usize::MAX).unwrap()
}

/// Rows by operation id.
fn by_op(rows: impl IntoIterator<Item = SnapshotEntry>) -> BTreeMap<String, SnapshotEntry> {
    rows.into_iter()
        .map(|row| (row.op_id.to_string(), row))
        .collect()
}

/// The registered root's operation log, off the runtime.
async fn op_log(root: &Path) -> Vec<OpEntry> {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let repo = Repo::open(&root, &test_settings()).await.unwrap();
            repo.op_log(10_000).await.unwrap()
        })
    })
    .await
    .unwrap()
}

fn report(daemon: &TestDaemon, repo: &SyntheticRepo) -> ReindexReport {
    *daemon
        .handle
        .state()
        .reindexed
        .lock()
        .unwrap()
        .get(&repo_id(repo))
        .expect("a start-time reindex report")
}

/// Stops `daemon`, runs `offline` on its database, and starts a new daemon
/// on the same state directory.
async fn restart_with(daemon: TestDaemon, offline: impl FnOnce(&Db)) -> TestDaemon {
    let TestDaemon {
        handle,
        client,
        tmp,
        config,
    } = daemon;
    drop(client);
    handle.shutdown();
    handle.join().await.unwrap();
    offline(&Db::open(&config.paths.db).unwrap());
    let handle = Daemon::start(config.clone()).await.unwrap();
    let client = Client::connect(handle.socket_path()).await.unwrap();
    TestDaemon {
        handle,
        client,
        tmp,
        config,
    }
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn every_kind_is_recorded() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, None).await;
    let watcher_op = watcher_snapshot(&daemon, &repo, &ws).await;
    quiesce(&daemon, &repo, &[&ws]).await;
    let explicit_op = explicit_snapshot(&daemon, &repo, &ws, "explicit.txt").await;
    let move_op = move_snapshot(&daemon, &repo, &ws).await;
    let final_op = final_snapshot(&daemon, &repo, &ws).await;

    let db = &daemon.handle.state().db;
    let rows = by_op(rows(db, &ws));
    let kinds: BTreeMap<_, _> = rows
        .iter()
        .map(|(op, row)| (op.clone(), row.kind))
        .collect();
    assert_eq!(
        kinds,
        BTreeMap::from([
            (watcher_op, SnapshotKind::Watcher),
            (explicit_op.clone(), SnapshotKind::Explicit),
            (move_op, SnapshotKind::Move),
            (final_op.clone(), SnapshotKind::Final),
        ])
    );
    assert!(rows.values().all(|row| row.local), "{rows:?}");
    assert!(
        rows.values()
            .all(|row| row.change_id.as_str() == ws.change_id)
    );
    assert!(rows.values().all(|row| row.workspace_id == id_of(&ws)));

    // One row per Reef snapshot operation of the workspace in the log, each
    // taken when its operation ended.
    let log: BTreeMap<String, OpEntry> = op_log(&repo.root)
        .await
        .into_iter()
        .filter(|op| is_reef_snapshot(op).is_some_and(|mark| mark.workspace_id == ws.id))
        .map(|op| (op.id.to_string(), op))
        .collect();
    assert_eq!(
        log.keys().collect::<BTreeSet<_>>(),
        rows.keys().collect::<BTreeSet<_>>()
    );
    for (op, row) in &rows {
        assert_eq!(row.taken_at, Timestamp::from(log[op].end), "{op}");
    }

    let inspected = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap();
    assert_eq!(inspected.snapshot_count, 4);
    assert_eq!(inspected.local_snapshot_count, 4);
    assert_eq!(inspected.workspace.unwrap().local_snapshot_count, 4);
    let recent: Vec<_> = inspected
        .recent_snapshots
        .iter()
        .map(|entry| (entry.op_id.as_str(), entry.kind.as_str(), entry.local))
        .collect();
    assert_eq!(recent.len(), 4);
    assert_eq!(
        recent[0],
        (final_op.as_str(), "final", true),
        "newest first"
    );
    let listed = daemon
        .client
        .list_workspaces(ListWorkspacesRequest::default())
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].local_snapshot_count, 4);
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn reindex_rebuilds() {
    let (daemon, repo) = fixture().await;
    let released = create(&daemon, &repo, None).await;
    let active = create(&daemon, &repo, None).await;
    watcher_snapshot(&daemon, &repo, &released).await;
    quiesce(&daemon, &repo, &[&released, &active]).await;
    explicit_snapshot(&daemon, &repo, &released, "explicit.txt").await;
    final_snapshot(&daemon, &repo, &released).await;
    // A custom description: the log marks it by its workspace name alone.
    write(&active, "other.txt", "other\n");
    let custom = daemon
        .client
        .snapshot_workspace(&repo.id, &active.id, "wip before refactor")
        .await
        .unwrap();
    assert!(custom.changed);
    let db = &daemon.handle.state().db;
    let before = by_op(rows(db, &released).into_iter().chain(rows(db, &active)));
    assert_eq!(before.len(), 4);

    let id = repo_id(&repo);
    let daemon = restart_with(daemon, |db| {
        assert_eq!(index::clear(db, id).unwrap(), 4);
    })
    .await;

    let db = &daemon.handle.state().db;
    let after = by_op(rows(db, &released).into_iter().chain(rows(db, &active)));
    assert_eq!(
        after.keys().collect::<Vec<_>>(),
        before.keys().collect::<Vec<_>>()
    );
    for (op, row) in &after {
        let old = &before[op];
        // The log cannot tell an explicit snapshot from a watcher one.
        let kind = match old.kind {
            SnapshotKind::Explicit => SnapshotKind::Watcher,
            kind => kind,
        };
        assert_eq!(
            row,
            &SnapshotEntry {
                kind,
                ..old.clone()
            },
            "{op}"
        );
    }
    let report = report(&daemon, &repo);
    assert_eq!(report.added, 4);
    assert_eq!(report.removed, 0);
    assert_eq!(report.skipped, 0);
    assert!(!report.truncated);
    assert!(report.walked >= 4, "{report:?}");
}

/// A row for an operation no log has: 64 bytes, like jj's ids.
fn fabricated(ws: &Workspace) -> SnapshotEntry {
    SnapshotEntry {
        op_id: "ab".repeat(64).parse().unwrap(),
        workspace_id: id_of(ws),
        commit_id: "cd".repeat(20).parse().unwrap(),
        change_id: ws.change_id.parse().unwrap(),
        kind: SnapshotKind::Watcher,
        local: true,
        taken_at: Timestamp::now(),
    }
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn reindex_removes_missing() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, None).await;
    quiesce(&daemon, &repo, &[&ws]).await;
    let real = explicit_snapshot(&daemon, &repo, &ws, "real.txt").await;
    let fake = fabricated(&ws);
    index::record(&daemon.handle.state().db, repo_id(&repo), &fake).unwrap();
    assert_eq!(rows(&daemon.handle.state().db, &ws).len(), 2);

    let daemon = daemon.restart().await;

    let db = &daemon.handle.state().db;
    assert_eq!(index::get(db, &fake.op_id).unwrap(), None);
    let left: Vec<_> = rows(db, &ws)
        .into_iter()
        .map(|row| row.op_id.to_string())
        .collect();
    assert_eq!(left, [real]);
    let report = report(&daemon, &repo);
    assert_eq!((report.added, report.removed), (0, 1), "{report:?}");
}

// 4
#[test]
fn no_writer_of_local_zero() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let migrations = src.join("db/migrations");
    let mut stack = vec![src];
    let mut offenders = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.starts_with(&migrations) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            // `local = 0`, `local=0`, and any other spacing or case.
            let squeezed: String = text
                .chars()
                .filter(|c| !c.is_whitespace())
                .flat_map(char::to_lowercase)
                .collect();
            if squeezed.contains("local=0") {
                offenders.push(path);
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "only R4's publish may clear `local`: {offenders:?}"
    );
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn for_change_lists_all_workspaces() {
    let (daemon, repo) = fixture().await;
    let a = create(&daemon, &repo, None).await;
    let b = create(
        &daemon,
        &repo,
        Some(create_request::Start::Change(a.change_id.clone())),
    )
    .await;
    quiesce(&daemon, &repo, &[&a, &b]).await;
    let a_op = explicit_snapshot(&daemon, &repo, &a, "from-a.txt").await;
    // `b` had its own edit when `a` rewrote the shared commit: it diverges,
    // on the same change.
    let b_op = explicit_snapshot(&daemon, &repo, &b, "from-b.txt").await;

    let rows = index::for_change(
        &daemon.handle.state().db,
        repo_id(&repo),
        &a.change_id.parse().unwrap(),
    )
    .unwrap();
    let found: BTreeSet<_> = rows
        .iter()
        .map(|row| (row.op_id.to_string(), row.workspace_id))
        .collect();
    assert_eq!(
        found,
        BTreeSet::from([(a_op, id_of(&a)), (b_op, id_of(&b))])
    );
}

// 6: a walk cut short cannot tell an operation past the limit from one
// that is gone, so it removes nothing.
#[tokio::test(flavor = "multi_thread")]
async fn truncated_walk_removes_nothing() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, None).await;
    quiesce(&daemon, &repo, &[&ws]).await;
    explicit_snapshot(&daemon, &repo, &ws, "one.txt").await;
    let state = daemon.handle.state();
    let fake = fabricated(&ws);
    index::record(&state.db, repo_id(&repo), &fake).unwrap();
    let handle = state.repos.get(repo_id(&repo)).unwrap();

    let report = index::reindex(&state.db, &handle, repo_id(&repo), 1)
        .await
        .unwrap();
    assert_eq!(
        report,
        ReindexReport {
            added: 0,
            removed: 0,
            skipped: 0,
            walked: 1,
            truncated: true
        }
    );
    assert!(index::get(&state.db, &fake.op_id).unwrap().is_some());

    let report = index::reindex(&state.db, &handle, repo_id(&repo), 100_000)
        .await
        .unwrap();
    assert!(!report.truncated);
    assert_eq!(report.removed, 1);
}
