//! Watcher-driven snapshots end to end (R1.13): the scheduler, the
//! staleness policy, and the ignore rules, against a registered synthetic
//! repository. Numbered as the brief numbers them; test 12 is reef-lib's
//! `sibling_operation_is_reported`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{SyntheticRepo, TestDaemon, test_settings};
use jj_lib::backend::CommitId;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use reef_daemon::model::WorkspaceId;
use reef_daemon::proto::v0::{CreateRequest, Workspace, create_request};
use reef_daemon::snapshot::PauseGuard;
use reef_daemon::workspace::SnapshotSpec;
use reef_lib::jj::oplog::OpEntry;
use reef_lib::jj::{Repo, ResolveError};
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";
const FALLBACK: Duration = Duration::from_secs(2);

/// A daemon with the brief's test intervals and a small registered repo.
/// Watched workspaces get the same short fallback, so the tests of lost
/// events need not wait the default ten minutes.
async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
}

async fn fixture_with(
    configure: impl FnOnce(&mut reef_daemon::config::Config),
) -> (TestDaemon, SyntheticRepo) {
    // `TEST_LOG=reef_daemon=debug` shows the daemons' logs.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("TEST_LOG"))
        .with_test_writer()
        .try_init();
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        let watcher = &mut config.file.watcher;
        watcher.debounce = Duration::from_millis(100);
        watcher.min_interval = Duration::from_millis(100);
        watcher.fallback_interval = FALLBACK;
        watcher.watched_fallback_interval = FALLBACK;
        configure(config);
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

fn write(ws: &Workspace, path: &str, contents: &str) {
    let path = Path::new(&ws.path).join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
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

/// Opens the registered root's own jj workspace at head, off the runtime.
async fn open(root: &Path) -> Repo {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(Repo::open(&root, &test_settings())).unwrap()
    })
    .await
    .unwrap()
}

async fn op_log(root: &Path) -> Vec<OpEntry> {
    let repo = open(root).await;
    tokio::task::spawn_blocking(move || pollster::block_on(repo.op_log(500)).unwrap())
        .await
        .unwrap()
}

/// The snapshot operations recorded for `ws`.
async fn snapshot_ops(root: &Path, ws: &Workspace) -> Vec<OpEntry> {
    let description = format!("reef: snapshot workspace {}", ws.id);
    op_log(root)
        .await
        .into_iter()
        .filter(|op| op.description == description)
        .collect()
}

/// Whether `path` is in `commit`'s tree.
async fn tree_has(root: &Path, commit: &str, path: &str) -> bool {
    let repo = open(root).await;
    let (commit, path) = (commit.to_owned(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let head = repo.head();
        let id = CommitId::try_from_hex(&commit).unwrap();
        let commit = head.store().get_commit(&id).unwrap();
        let path = RepoPathBuf::from_internal_string(path).unwrap();
        !pollster::block_on(commit.tree().path_value(&path))
            .unwrap()
            .is_absent()
    })
    .await
    .unwrap()
}

/// Polls `check` every 50 ms until it holds, failing after `bound`.
async fn wait_for<F, Fut>(bound: Duration, what: &str, mut check: F) -> Duration
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let begun = Instant::now();
    loop {
        if check().await {
            return begun.elapsed();
        }
        assert!(begun.elapsed() < bound, "{what}: not within {bound:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn write_is_snapshotted_within_debounce() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, None).await;
    assert_eq!(ws.last_snapshot_op, "");
    write(&ws, "agent-wrote-this.txt", "hello\n");
    let waited = wait_for(Duration::from_secs(3), "a watcher snapshot", || async {
        !inspect(&daemon, &repo, &ws)
            .await
            .last_snapshot_op
            .is_empty()
    })
    .await;
    eprintln!("write reached the operation log in {waited:.2?}");

    let row = inspect(&daemon, &repo, &ws).await;
    let log = op_log(&repo.root).await;
    let op = log
        .iter()
        .find(|op| op.id.as_str() == row.last_snapshot_op)
        .expect("the snapshot's operation is in the log");
    assert!(op.is_snapshot);
    assert_eq!(
        op.description,
        format!("reef: snapshot workspace {}", ws.id)
    );
    assert!(tree_has(&repo.root, &row.commit_id, "agent-wrote-this.txt").await);
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn burst_coalesces() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, None).await;
    let begun = Instant::now();
    for i in 0..1000 {
        write(&ws, &format!("burst/{}/{i}.txt", i % 10), "x\n");
    }
    let burst = begun.elapsed();
    assert!(
        burst < Duration::from_millis(1500),
        "the burst took {burst:?}"
    );
    tokio::time::sleep(Duration::from_secs(3)).await;
    let ops = snapshot_ops(&repo.root, &ws).await;
    assert!(
        (1..=3).contains(&ops.len()),
        "{} snapshot operations for a burst written in {burst:?}",
        ops.len()
    );
    let row = inspect(&daemon, &repo, &ws).await;
    assert!(tree_has(&repo.root, &row.commit_id, "burst/9/999.txt").await);
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn excluded_paths_do_not_trigger() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, None).await;
    let snapshots = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (sink, id) = (std::sync::Arc::clone(&snapshots), id_of(&ws));
    daemon
        .handle
        .state()
        .manager
        .scheduler()
        .on_snapshot(Box::new(move |outcome| {
            if outcome.workspace == id {
                sink.lock().unwrap().push(outcome.paths_count);
            }
        }));
    // What R1.19 reads from `reef.toml` at create.
    daemon
        .handle
        .state()
        .manager
        .set_snapshot_spec(
            id_of(&ws),
            SnapshotSpec {
                excludes: vec!["target/".to_owned()],
                mounts: vec![],
            },
        )
        .await
        .unwrap();
    for i in 0..100 {
        write(&ws, &format!("target/debug/{i}.o"), "obj\n");
    }
    tokio::time::sleep(FALLBACK + Duration::from_secs(1)).await;
    // The scan ignores the files too, so an operation count cannot tell a
    // triggered snapshot from none: the hook sees what triggered each one.
    let seen = snapshots.lock().unwrap().clone();
    assert!(
        seen.iter().all(|paths| *paths == 0),
        "no snapshot was triggered by events: {seen:?}"
    );
    assert!(!seen.is_empty(), "the fallback snapshot ran");
    assert_eq!(snapshot_ops(&repo.root, &ws).await.len(), 0);
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(!snapshot.changed, "excluded files are not snapshotted");
    assert!(!tree_has(&repo.root, &snapshot.commit_id, "target/debug/0.o").await);
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn reefignore_is_honored() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, None).await;
    write(&ws, ".reefignore", "scratch/\n");
    write(&ws, "scratch/notes.txt", "private\n");
    write(&ws, "kept.txt", "kept\n");
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let commit = snapshot.commit_id;
    assert!(tree_has(&repo.root, &commit, "kept.txt").await);
    assert!(tree_has(&repo.root, &commit, ".reefignore").await);
    assert!(!tree_has(&repo.root, &commit, "scratch/notes.txt").await);
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn fallback_timer_catches_missed_events() {
    let (daemon, repo) = fixture().await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    let ws = create(&daemon, &repo, None).await;
    write(&ws, "missed.txt", "the watcher never saw this\n");
    let waited = wait_for(
        FALLBACK + Duration::from_secs(1),
        "a fallback snapshot",
        || async {
            !inspect(&daemon, &repo, &ws)
                .await
                .last_snapshot_op
                .is_empty()
        },
    )
    .await;
    eprintln!("fallback snapshot after {waited:.2?}");
    let row = inspect(&daemon, &repo, &ws).await;
    assert!(tree_has(&repo.root, &row.commit_id, "missed.txt").await);
}

/// Two workspaces on one change: `a` a new change on the default start,
/// `b` editing `a`'s change.
async fn two_on_one_change(daemon: &TestDaemon, repo: &SyntheticRepo) -> (Workspace, Workspace) {
    let a = create(daemon, repo, None).await;
    let b = create(
        daemon,
        repo,
        Some(create_request::Start::Change(a.change_id.clone())),
    )
    .await;
    assert_eq!(a.commit_id, b.commit_id);
    (a, b)
}

/// Holds off automatic snapshots of `a` and `b`, then waits out any already
/// in flight (an explicit snapshot runs after it), so the test's own
/// snapshots see its writes first. Under load, creating `b` can outlast
/// `a`'s fallback interval.
async fn hold_both(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    a: &Workspace,
    b: &Workspace,
) -> (PauseGuard, PauseGuard) {
    let scheduler = daemon.handle.state().manager.scheduler();
    let hold = (scheduler.pause(id_of(a)), scheduler.pause(id_of(b)));
    for ws in [a, b] {
        daemon
            .client
            .snapshot_workspace(&repo.id, &ws.id, "")
            .await
            .unwrap();
    }
    hold
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn stale_adopts_when_clean() {
    let (daemon, repo) = fixture().await;
    let (a, b) = two_on_one_change(&daemon, &repo).await;
    let _hold = hold_both(&daemon, &repo, &a, &b).await;
    write(&a, "from-a.txt", "a\n");
    let snapshot_a = daemon
        .client
        .snapshot_workspace(&repo.id, &a.id, "")
        .await
        .unwrap();
    assert!(snapshot_a.changed);

    let ops_before = op_log(&repo.root).await.len();
    let snapshot_b = daemon
        .client
        .snapshot_workspace(&repo.id, &b.id, "")
        .await
        .unwrap();
    assert!(!snapshot_b.changed);
    assert_eq!(snapshot_b.commit_id, snapshot_a.commit_id);
    assert_eq!(
        op_log(&repo.root).await.len(),
        ops_before,
        "no operation was written for b"
    );
    assert_eq!(
        std::fs::read_to_string(Path::new(&b.path).join("from-a.txt")).unwrap(),
        "a\n"
    );
    let row = inspect(&daemon, &repo, &b).await;
    assert!(!row.divergent);
    assert_eq!(row.attention, "");
    assert_eq!(row.commit_id, snapshot_a.commit_id);
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn stale_diverges_when_dirty() {
    let (daemon, repo) = fixture().await;
    let (a, b) = two_on_one_change(&daemon, &repo).await;
    let hold = hold_both(&daemon, &repo, &a, &b).await;
    write(&a, "from-a.txt", "a\n");
    write(&b, "from-b.txt", "b\n");
    let snapshot_a = daemon
        .client
        .snapshot_workspace(&repo.id, &a.id, "")
        .await
        .unwrap();
    let snapshot_b = daemon
        .client
        .snapshot_workspace(&repo.id, &b.id, "")
        .await
        .unwrap();
    assert!(snapshot_a.changed && snapshot_b.changed);
    drop(hold);

    let row_a = inspect(&daemon, &repo, &a).await;
    let row_b = inspect(&daemon, &repo, &b).await;
    assert!(!row_a.divergent);
    assert!(row_b.divergent);
    assert_eq!(row_b.attention, "divergent");
    assert_eq!(row_a.change_id, row_b.change_id);

    let error = open(&repo.root)
        .await
        .resolve_revision(&a.change_id)
        .await
        .unwrap_err();
    let ResolveError::DivergentChange { commits, .. } = error else {
        panic!("expected a divergent change, got {error:?}");
    };
    let commits: BTreeSet<String> = commits.iter().map(ToString::to_string).collect();
    assert_eq!(
        commits,
        BTreeSet::from([snapshot_a.commit_id.clone(), snapshot_b.commit_id.clone()])
    );
    let (ca, cb) = (&snapshot_a.commit_id, &snapshot_b.commit_id);
    assert!(tree_has(&repo.root, ca, "from-a.txt").await);
    assert!(!tree_has(&repo.root, ca, "from-b.txt").await);
    assert!(tree_has(&repo.root, cb, "from-b.txt").await);
    assert!(!tree_has(&repo.root, cb, "from-a.txt").await);
    assert!(!Path::new(&b.path).join("from-a.txt").exists());
}

// 8. The daemon sees the outside operation when it refreshes its head
// before the scan, so the scan finds the working copy fresh; the
// `RepoBehind` answer itself (an outside write between refresh and scan)
// is reef-lib's `stale_working_copy_detected` and
// `sibling_operation_is_reported`.
#[tokio::test(flavor = "multi_thread")]
async fn repo_behind_is_retried() {
    let (daemon, repo) = fixture().await;
    let b = create(&daemon, &repo, None).await;
    let _hold = daemon.handle.state().manager.scheduler().pause(id_of(&b));
    // Snapshot b through a second jj handle, as the `jj` CLI would: its
    // working copy moves past the daemon's head.
    write(&b, "outside.txt", "written outside the daemon\n");
    let state_dir = daemon
        .state_dir()
        .join("repos")
        .join(&repo.id)
        .join("wc")
        .join(&b.id);
    let (root, ws_root) = (repo.root.clone(), PathBuf::from(&b.path));
    let outside = tokio::task::spawn_blocking(move || {
        let main = pollster::block_on(Repo::open(&root, &test_settings())).unwrap();
        let mut ws = main.open_workspace(&ws_root, &state_dir).unwrap();
        pollster::block_on(ws.snapshot("outside the daemon")).unwrap()
    })
    .await
    .unwrap();
    assert!(outside.changed);

    write(&b, "inside.txt", "written after\n");
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &b.id, "")
        .await
        .unwrap();
    assert!(snapshot.changed);
    let log = op_log(&repo.root).await;
    let op = log
        .iter()
        .find(|op| op.id.as_str() == snapshot.op_id)
        .unwrap();
    assert_eq!(
        op.parents,
        std::slice::from_ref(&outside.op_id),
        "built on the newer operation"
    );
    assert!(tree_has(&repo.root, &snapshot.commit_id, "outside.txt").await);
    assert!(tree_has(&repo.root, &snapshot.commit_id, "inside.txt").await);
    assert!(!inspect(&daemon, &repo, &b).await.divergent);
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn forgotten_workspace_is_recovered() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, None).await;
    let _hold = daemon.handle.state().manager.scheduler().pause(id_of(&ws));
    let name = format!("reef-{}", ws.id);
    let root = repo.root.clone();
    tokio::task::spawn_blocking(move || {
        let mut main = pollster::block_on(Repo::open(&root, &test_settings())).unwrap();
        pollster::block_on(main.forget_workspace(&name, "forget it behind the daemon's back"))
            .unwrap();
    })
    .await
    .unwrap();
    write(&ws, "after-forget.txt", "kept\n");

    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(snapshot.changed);
    let row = inspect(&daemon, &repo, &ws).await;
    assert_eq!(row.attention, "stale-recovered");
    assert_ne!(row.change_id, ws.change_id, "a new change");
    assert_eq!(row.commit_id, snapshot.commit_id);
    assert!(tree_has(&repo.root, &snapshot.commit_id, "after-forget.txt").await);
    let wc: Vec<String> = open(&repo.root)
        .await
        .workspaces_on_commit(&snapshot.commit_id.parse().unwrap());
    assert_eq!(wc, [format!("reef-{}", ws.id)]);
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn concurrency_is_bounded() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.watcher.max_concurrent_scans = Some(2);
    })
    .await;
    let mut workspaces = Vec::new();
    for _ in 0..20 {
        workspaces.push(create(&daemon, &repo, None).await);
    }
    let begun = Instant::now();
    for ws in &workspaces {
        write(ws, "parallel.txt", &ws.id);
    }
    for ws in &workspaces {
        let left = Duration::from_secs(10).saturating_sub(begun.elapsed());
        wait_for(left, "every workspace snapshotted", || async {
            !inspect(&daemon, &repo, ws)
                .await
                .last_snapshot_op
                .is_empty()
        })
        .await;
    }
    eprintln!("20 workspaces snapshotted in {:.2?}", begun.elapsed());
    let scheduler = daemon.handle.state().manager.scheduler();
    assert!(
        (1..=2).contains(&scheduler.max_running()),
        "{} scans ran at once",
        scheduler.max_running()
    );
}

// 11
#[cfg(target_os = "linux")]
#[test]
fn inotify_limits_logged() {
    let (output, _state) = common::run_binary(&[("REEF_LOG_FORMAT", "json")]);
    let stderr = String::from_utf8(output.stderr).unwrap();
    let limits = stderr
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|event| event["message"] == "inotify limits")
        .unwrap_or_else(|| panic!("no inotify limits event in:\n{stderr}"));
    assert!(limits["watches_count"].as_u64().is_some(), "{limits}");
    assert!(limits["instances_count"].as_u64().is_some(), "{limits}");
}

// A workspace that cannot be watched (its watcher failed to start) is
// marked `watch-failed` and still snapshotted by the fallback timer.
#[tokio::test(flavor = "multi_thread")]
async fn no_watcher_falls_back_to_the_timer() {
    let (daemon, repo) = fixture().await;
    daemon.handle.state().manager.scheduler().refuse_watches();
    let ws = create(&daemon, &repo, None).await;
    assert_eq!(inspect(&daemon, &repo, &ws).await.attention, "watch-failed");
    write(&ws, "unwatched.txt", "x\n");
    wait_for(
        FALLBACK + Duration::from_secs(1),
        "a fallback snapshot",
        || async {
            !inspect(&daemon, &repo, &ws)
                .await
                .last_snapshot_op
                .is_empty()
        },
    )
    .await;
    let row = inspect(&daemon, &repo, &ws).await;
    assert!(tree_has(&repo.root, &row.commit_id, "unwatched.txt").await);
}

// A workspace whose watch works waits `watched_fallback_interval` for a
// fallback snapshot, so idle watched workspaces are not all rescanned every
// `fallback_interval` (R1.23); one whose watch failed waits only
// `fallback_interval`.
#[tokio::test(flavor = "multi_thread")]
async fn only_unwatched_workspaces_get_the_short_fallback() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
    })
    .await;
    let scheduler = daemon.handle.state().manager.scheduler();
    // Events are lost, so only a fallback snapshot can take the writes.
    scheduler.pause_watcher();
    let watched = create(&daemon, &repo, None).await;
    scheduler.refuse_watches();
    let unwatched = create(&daemon, &repo, None).await;
    assert_eq!(inspect(&daemon, &repo, &watched).await.attention, "");
    assert_eq!(
        inspect(&daemon, &repo, &unwatched).await.attention,
        "watch-failed"
    );
    write(&watched, "watched.txt", "x\n");
    write(&unwatched, "unwatched.txt", "x\n");
    wait_for(
        FALLBACK + Duration::from_secs(1),
        "a fallback snapshot of the unwatched workspace",
        || async {
            !inspect(&daemon, &repo, &unwatched)
                .await
                .last_snapshot_op
                .is_empty()
        },
    )
    .await;
    tokio::time::sleep(FALLBACK).await;
    assert_eq!(
        inspect(&daemon, &repo, &watched).await.last_snapshot_op,
        "",
        "the watched workspace was not rescanned after {FALLBACK:?} twice over"
    );
}

// Post-snapshot hooks see every snapshot that ends well, after the row.
#[tokio::test(flavor = "multi_thread")]
async fn hooks_run_after_snapshots() {
    let (daemon, repo) = fixture().await;
    let state = daemon.handle.state();
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (db, sink) = (
        std::sync::Arc::clone(&state.db),
        std::sync::Arc::clone(&seen),
    );
    state
        .manager
        .scheduler()
        .on_snapshot(Box::new(move |outcome| {
            let row = db.workspace(outcome.workspace).unwrap();
            sink.lock()
                .unwrap()
                .push((outcome.clone(), row.last_snapshot_op));
        }));
    let ws = create(&daemon, &repo, None).await;
    let _hold = state.manager.scheduler().pause(id_of(&ws));
    write(&ws, "hooked.txt", "x\n");
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let seen = seen.lock().unwrap();
    let (outcome, row_op) = seen.last().expect("the hook ran");
    assert!(outcome.changed);
    assert_eq!(outcome.kind, reef_daemon::snapshot::SnapshotKind::Explicit);
    assert_eq!(outcome.op_id.to_string(), snapshot.op_id);
    assert_eq!(
        row_op.as_deref(),
        Some(snapshot.op_id.as_str()),
        "the row came first"
    );
}

// A released workspace is no longer tracked, so no watcher snapshot races
// its tear-down.
#[tokio::test(flavor = "multi_thread")]
async fn release_untracks() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, None).await;
    let scheduler = daemon.handle.state().manager.scheduler();
    assert!(scheduler.is_tracked(id_of(&ws)));
    daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    assert!(!scheduler.is_tracked(id_of(&ws)));
}

// Acceptance criterion (manual, numbers in the PR): with 20 active
// workspaces on the medium preset and the default watcher configuration, a
// write reaches the operation log within 2 s. Run with
// `cargo test --release -p reef-daemon --test snapshots -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual measurement on the medium preset; prints its numbers"]
async fn medium_write_latency_with_twenty_workspaces() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo(Preset::Medium).await;
    let mut workspaces = Vec::new();
    for _ in 0..20 {
        workspaces.push(create(&daemon, &repo, None).await);
    }
    // Let creation settle: no snapshot is pending anywhere.
    tokio::time::sleep(Duration::from_secs(2)).await;

    let mut one_at_a_time = Vec::new();
    for ws in workspaces.iter().take(5) {
        write(ws, "latency.txt", "one\n");
        let waited = wait_for(Duration::from_secs(10), "a watcher snapshot", || async {
            !inspect(&daemon, &repo, ws)
                .await
                .last_snapshot_op
                .is_empty()
        })
        .await;
        one_at_a_time.push(waited);
    }

    let mut before = Vec::new();
    for ws in &workspaces {
        before.push(inspect(&daemon, &repo, ws).await.last_snapshot_op);
    }
    let begun = Instant::now();
    for ws in &workspaces {
        write(ws, "together.txt", "all\n");
    }
    let mut all_at_once = Vec::new();
    for (ws, before) in workspaces.iter().zip(&before) {
        let left = Duration::from_secs(20).saturating_sub(begun.elapsed());
        wait_for(left, "every workspace snapshotted", || async {
            let row = inspect(&daemon, &repo, ws).await;
            row.last_snapshot_op != *before
                && tree_has(&repo.root, &row.commit_id, "together.txt").await
        })
        .await;
        all_at_once.push(begun.elapsed());
    }
    eprintln!("medium preset, 20 active workspaces, default watcher configuration");
    eprintln!("one write at a time, write to operation log: {one_at_a_time:.2?}");
    eprintln!(
        "a write in all 20 at once, last one in the operation log after {:.2?}",
        all_at_once.iter().max().unwrap()
    );
    assert!(
        one_at_a_time
            .iter()
            .all(|waited| *waited < Duration::from_secs(2))
    );
}

/// R1.06: a moved workspace is still watched (an overlay's is a new mount),
/// and the files the move itself wrote do not land in the new change.
#[tokio::test(flavor = "multi_thread")]
async fn moved_workspace_is_still_watched() {
    let (daemon, repo) = fixture().await;
    let other = create(&daemon, &repo, None).await;
    write(&other, "theirs.txt", "a second tree\n");
    let target = daemon
        .client
        .snapshot_workspace(&repo.id, &other.id, "")
        .await
        .unwrap()
        .commit_id;
    let ws = create(&daemon, &repo, None).await;
    let moved = daemon
        .client
        .move_workspace(reef_daemon::proto::v0::MoveRequest {
            repo: repo.id.clone(),
            workspace: ws.id.clone(),
            target: Some(reef_daemon::proto::v0::move_request::Target::NewOn(target)),
            ..Default::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();

    write(&ws, "after-move.txt", "seen by the watcher\n");
    wait_for(Duration::from_secs(5), "a watcher snapshot", || async {
        let row = inspect(&daemon, &repo, &ws).await;
        row.commit_id != moved.commit_id
            && tree_has(&repo.root, &row.commit_id, "after-move.txt").await
    })
    .await;
    let row = inspect(&daemon, &repo, &ws).await;
    assert_eq!(row.change_id, moved.change_id, "the change moved to");
    assert!(tree_has(&repo.root, &row.commit_id, "theirs.txt").await);
}

/// R1.29: a write made the moment `Create` returns is snapshotted by the
/// watcher, which is attached by then, not by the fallback timer. Tracking
/// starts the `min_interval` clock, so the first snapshot can start that
/// long after the create; the Watchman variant is in `tests/watchman.rs`.
#[tokio::test(flavor = "multi_thread")]
async fn write_right_after_create_is_snapshotted() {
    let (daemon, repo) = fixture_with(|config| {
        let watcher = &mut config.file.watcher;
        watcher.backend = reef_daemon::config::WatcherBackend::Notify;
        watcher.fallback_interval = Duration::from_mins(10);
    })
    .await;
    let watcher = &daemon.config.file.watcher;
    let bound = watcher.min_interval + watcher.debounce + Duration::from_secs(2);
    let ws = create(&daemon, &repo, None).await;
    write(&ws, "right-after-create.txt", "hello\n");
    assert_eq!(ws.watcher, "notify");
    let waited = wait_for(bound, "a watcher snapshot", || async {
        let row = inspect(&daemon, &repo, &ws).await;
        !row.last_snapshot_op.is_empty()
            && tree_has(&repo.root, &row.commit_id, "right-after-create.txt").await
    })
    .await;
    eprintln!("a write right after create reached the operation log in {waited:.2?}");
}

// R1.30, 1: a watcher snapshot reports how its scan split between jj's
// working-copy load and the walk, both inside `scan_ms`.
#[tokio::test(flavor = "multi_thread")]
async fn scan_split_is_reported() {
    let (daemon, repo) = fixture().await;
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&seen);
    daemon
        .handle
        .state()
        .manager
        .scheduler()
        .on_snapshot(Box::new(move |outcome| {
            sink.lock().unwrap().push(outcome.clone());
        }));
    let ws = create(&daemon, &repo, None).await;
    write(&ws, "split.txt", "x\n");
    let id = id_of(&ws);
    let watcher = || {
        seen.lock()
            .unwrap()
            .iter()
            .rev()
            .find(|outcome| {
                outcome.workspace == id
                    && outcome.kind == reef_daemon::snapshot::SnapshotKind::Watcher
                    && outcome.changed
            })
            .cloned()
    };
    wait_for(Duration::from_secs(10), "a watcher snapshot", || async {
        watcher().is_some()
    })
    .await;
    let outcome = watcher().unwrap();
    assert!(
        outcome.load_ms + outcome.walk_ms <= outcome.scan_ms,
        "load {} + walk {} > scan {}",
        outcome.load_ms,
        outcome.walk_ms,
        outcome.scan_ms
    );
}
