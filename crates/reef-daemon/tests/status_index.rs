//! The incremental status index (R2.23, ADR 0048): reads that snapshot a
//! workspace first answer from jj's working-copy state and the watcher's
//! journal, synced to each read. Numbered as the brief numbers them; 20's
//! unit half and the tap's are in `watcher/notify.rs`, the journal's in
//! `snapshot/journal.rs`, 22 in `watchman.rs`.
//!
//! The watcher runs with a short debounce; both fallback timers are an
//! hour unless a test sets them, so no fallback snapshot stands in for the
//! read under test. Tests wait for conditions and never assert durations.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{SyntheticRepo, TestDaemon};
use reef_api::v1::{
    CreateWorkspaceRequest, GetStatusRequest, StatusSource, StatusWalkReason, Workspace,
    WorkspaceStatus,
};
use reef_daemon::model::WorkspaceId;
use reef_daemon::snapshot::{SnapshotKind, SnapshotOutcome};
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";
/// How long a condition may take: debug builds are slow under load.
const BOUND: Duration = Duration::from_secs(30);

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
        watcher.backend = reef_daemon::config::WatcherBackend::Notify;
        watcher.debounce = Duration::from_millis(100);
        watcher.min_interval = Duration::from_millis(100);
        watcher.fallback_interval = Duration::from_hours(1);
        watcher.watched_fallback_interval = Duration::from_hours(1);
        // Watched before `Create` returns on both platforms, so the first
        // read is the index's; test 6 turns it back on.
        watcher.notify_attach_background = false;
        configure(config);
    })
    .await
}

async fn create(daemon: &TestDaemon, repo: &SyntheticRepo) -> Workspace {
    daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            ..CreateWorkspaceRequest::default()
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

/// The workspace's status, from the index unless `skip`.
async fn status_of(daemon: &TestDaemon, ws: &Workspace, skip: bool) -> WorkspaceStatus {
    daemon
        .client
        .get_status(GetStatusRequest {
            workspace: ws.id.clone(),
            skip_snapshot: skip,
            max_files: 10_000,
            ..GetStatusRequest::default()
        })
        .await
        .unwrap()
        .workspace_status
        .unwrap()
}

/// The changed paths of a status.
fn changed(status: &WorkspaceStatus) -> BTreeSet<String> {
    status
        .changed_files
        .iter()
        .map(|file| file.path.clone())
        .collect()
}

/// A status's source and walk reason.
fn source(status: &WorkspaceStatus) -> (StatusSource, StatusWalkReason) {
    (status.source(), status.walk_reason())
}

const INDEX: (StatusSource, StatusWalkReason) =
    (StatusSource::Index, StatusWalkReason::Unspecified);

fn walk(reason: StatusWalkReason) -> (StatusSource, StatusWalkReason) {
    (StatusSource::Walk, reason)
}

/// Every snapshot outcome of one workspace, from a post-snapshot hook.
#[derive(Clone)]
struct Outcomes(Arc<Mutex<Vec<SnapshotOutcome>>>);

impl Outcomes {
    fn record(daemon: &TestDaemon, ws: &Workspace) -> Self {
        let outcomes = Self(Arc::default());
        let (sink, id) = (Arc::clone(&outcomes.0), id_of(ws));
        daemon
            .handle
            .state()
            .manager
            .scheduler()
            .on_snapshot(Box::new(move |outcome| {
                if outcome.workspace == id {
                    sink.lock().unwrap().push(outcome.clone());
                }
            }));
        outcomes
    }

    fn all(&self) -> Vec<SnapshotOutcome> {
        self.0.lock().unwrap().clone()
    }

    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }
}

/// Waits for `check`, failing after [`BOUND`].
async fn until(what: &str, mut check: impl AsyncFnMut() -> bool) {
    let begun = Instant::now();
    while !check().await {
        assert!(begun.elapsed() < BOUND, "{what}: not within {BOUND:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits until the watcher's snapshots of `ws` have settled: a status takes
/// no snapshot.
async fn settle(daemon: &TestDaemon, ws: &Workspace) {
    tokio::time::sleep(Duration::from_millis(300)).await;
    until("the workspace settles", async || {
        let status = status_of(daemon, ws, false).await;
        status.snapshot.is_none() && source(&status) == INDEX
    })
    .await;
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn clean_status_takes_no_snapshot() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    write(&ws, "one.txt", "1\n");
    settle(&daemon, &ws).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let status = status_of(&daemon, &ws, false).await;
    assert_eq!(source(&status), INDEX, "{status:?}");
    assert!(status.snapshot.is_none());
    assert_eq!(changed(&status), BTreeSet::from(["one.txt".to_owned()]));
    daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, true)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 0, "no read scanned: {:#?}", outcomes.all());
}

/// One edit of each kind the index must see, by step.
fn edit(ws: &Workspace, step: usize) -> Vec<String> {
    let root = Path::new(&ws.path);
    let name = format!("e-{step}.txt");
    match step % 9 {
        0 => {
            write(ws, &name, "created\n");
            vec![name]
        }
        1 => {
            write(ws, "appended.txt", &"x".repeat(step));
            vec!["appended.txt".to_owned()]
        }
        2 => {
            write(ws, "appended.txt", "");
            vec!["appended.txt".to_owned()]
        }
        3 => {
            let _ = std::fs::remove_file(root.join("appended.txt"));
            vec!["appended.txt".to_owned()]
        }
        4 => {
            write(ws, "to-rename.txt", &format!("r{step}\n"));
            std::fs::rename(root.join("to-rename.txt"), root.join(&name)).unwrap();
            vec![name]
        }
        5 | 6 => {
            use std::os::unix::fs::PermissionsExt as _;
            write(ws, "mode.sh", "#!/bin/sh\n");
            let mode = if step % 9 == 5 { 0o755 } else { 0o644 };
            std::fs::set_permissions(root.join("mode.sh"), std::fs::Permissions::from_mode(mode))
                .unwrap();
            vec!["mode.sh".to_owned()]
        }
        7 => {
            let link = root.join(format!("link-{step}"));
            std::os::unix::fs::symlink("target-of-link", &link).unwrap();
            vec![format!("link-{step}")]
        }
        _ => {
            let path = format!("dir-{step}/nested/file.txt");
            write(ws, &path, "nested\n");
            vec![path]
        }
    }
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn status_right_after_a_write() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let rounds = std::env::var("REEF_INDEX_ROUNDS")
        .ok()
        .and_then(|rounds| rounds.parse().ok())
        .unwrap_or(108);
    for step in 0..rounds {
        let paths = edit(&ws, step);
        let before = outcomes.len();
        let status = status_of(&daemon, &ws, false).await;
        assert_eq!(source(&status), INDEX, "step {step}: {status:?}");
        // The walk finds nothing the index missed.
        let full = daemon
            .client
            .snapshot_workspace(&repo.id, &ws.id, "")
            .await
            .unwrap();
        assert!(full.operation.is_none(), "step {step}: {full:?}");
        let listed = changed(&status);
        for path in paths {
            if Path::new(&ws.path).join(&path).symlink_metadata().is_ok() {
                assert!(
                    listed.contains(&path),
                    "step {step}: {path} not in {listed:?}"
                );
            }
        }
        // The read's own snapshot, when it took one, was narrowed.
        let read = outcomes.all()[before..]
            .iter()
            .find(|outcome| outcome.kind == SnapshotKind::Explicit)
            .cloned();
        if let Some(read) = read.filter(|_| status.snapshot.is_some()) {
            assert!(read.narrowed, "step {step}: {read:?}");
        }
    }
}

/// A small PRNG, so the differential test replays by seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        usize::try_from(self.0 % u64::try_from(n).unwrap()).unwrap()
    }
}

/// Renames across the root's edge, unless the root is its own mount (an
/// overlay backend), where the kernel refuses with `EXDEV`.
fn cross_rename(from: &Path, to: &Path) {
    match std::fs::rename(from, to) {
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {}
        other => other.unwrap(),
    }
}

/// Writes `hidden/seen.txt` and waits until a write under `hidden/` is
/// reported, so its watch is known to be up (on Linux a new directory
/// stays dirty until then), and the workspace settles.
async fn hidden_dir(daemon: &TestDaemon, ws: &Workspace) {
    write(ws, "hidden/seen.txt", "s\n");
    settle(daemon, ws).await;
    let journals = daemon.handle.state().manager.scheduler().journals();
    let id = id_of(ws);
    let mut n = 0;
    until("an event from under hidden/", async || {
        n += 1;
        write(ws, &format!("hidden/again-{n}.txt"), "a\n");
        tokio::time::sleep(Duration::from_millis(100)).await;
        journals.journal_of(id).is_some_and(|(paths, _)| {
            paths
                .iter()
                .any(|path| path.ends_with(format!("again-{n}.txt")))
        })
    })
    .await;
    settle(daemon, ws).await;
}

/// Larger than the differential test's `snapshot.max_new_file_size`.
const BIG: usize = 80 << 10;

/// One random step of the differential test.
fn random_step(ws: &Workspace, rng: &mut Rng, step: usize) {
    use std::os::unix::fs::PermissionsExt as _;
    let root = Path::new(&ws.path);
    let pick = |rng: &mut Rng| format!("d{}/f{}.txt", rng.next(3), rng.next(5));
    match rng.next(15) {
        0 => write(ws, &pick(rng), &format!("new {step}\n")),
        1 => write(ws, &format!("deep{}/a/b/c.txt", rng.next(3)), "deep\n"),
        2 => {
            // Same size.
            let path = pick(rng);
            if root.join(&path).exists() {
                write(ws, &path, &format!("{:>8}\n", step % 1000));
            }
        }
        3 => {
            let _ = std::fs::remove_file(root.join(pick(rng)));
        }
        4 => {
            let (from, to) = (pick(rng), pick(rng));
            if root.join(&from).is_file() && !root.join(&to).exists() {
                std::fs::create_dir_all(root.join(&to).parent().unwrap()).unwrap();
                std::fs::rename(root.join(from), root.join(to)).unwrap();
            }
        }
        5 => {
            let (from, to) = (format!("d{}", rng.next(3)), format!("moved{step}"));
            if root.join(&from).is_dir() && !root.join(&to).exists() {
                std::fs::rename(root.join(from), root.join(to)).unwrap();
            }
        }
        6 => {
            // A directory moved in from outside the root.
            let outside = root.parent().unwrap().join(format!("outside-{step}"));
            let inside = root.join(format!("in-{step}"));
            if !inside.exists() {
                std::fs::create_dir_all(&outside).unwrap();
                std::fs::write(outside.join("in.txt"), "moved in\n").unwrap();
                cross_rename(&outside, &inside);
            }
        }
        7 => {
            let path = pick(rng);
            if root.join(&path).is_file() {
                let mode = if rng.next(2) == 0 { 0o755 } else { 0o644 };
                std::fs::set_permissions(root.join(path), std::fs::Permissions::from_mode(mode))
                    .unwrap();
            }
        }
        8 => {
            let link = root.join(format!("l{}", rng.next(3)));
            let _ = std::fs::remove_file(&link);
            if rng.next(2) == 0 {
                std::os::unix::fs::symlink(format!("t{step}"), &link).unwrap();
            } else {
                std::fs::write(&link, "now a file\n").unwrap();
            }
        }
        9 => write(ws, &format!("ignored-{step}.log"), "ignored\n"),
        10 => write(ws, ".gitignore", &format!("*.log\n# {step}\n")),
        11 => {
            // A directory moved out of the root.
            let from = root.join(format!("d{}", rng.next(3)));
            if from.is_dir() {
                let outside = root.parent().unwrap().join(format!("moved-out-{step}"));
                cross_rename(&from, &outside);
            }
        }
        12 => write(ws, &format!("big-{step}.bin"), &"b".repeat(BIG)),
        13 => {
            // Case only: one file on a case-insensitive volume.
            let path = pick(rng);
            let upper = path.replace(".txt", ".TXT");
            if root.join(&path).is_file() && !root.join(&upper).exists() {
                std::fs::rename(root.join(path), root.join(upper)).unwrap();
            }
        }
        _ => std::fs::create_dir_all(root.join(format!("empty{}", rng.next(3)))).unwrap(),
    }
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn index_status_matches_walk() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.snapshot.max_new_file_size = reef_daemon::config::ByteSize(64 << 10);
    })
    .await;
    let journals = Arc::clone(daemon.handle.state().manager.scheduler().journals());
    let seeds: u64 = std::env::var("REEF_INDEX_SEEDS")
        .ok()
        .and_then(|seeds| seeds.parse().ok())
        .unwrap_or(3);
    let steps = if seeds > 3 { 100 } else { 40 };
    for seed in 1..=seeds {
        let ws = create(&daemon, &repo).await;
        write(&ws, ".gitignore", "*.log\n");
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
        for step in 0..steps {
            for _ in 0..=rng.next(3) {
                random_step(&ws, &mut rng, step);
            }
            let index = status_of(&daemon, &ws, false).await;
            assert_eq!(source(&index), INDEX, "seed {seed} step {step}: {index:?}");
            let full = daemon
                .client
                .snapshot_workspace(&repo.id, &ws.id, "")
                .await
                .unwrap();
            assert!(
                full.operation.is_none(),
                "seed {seed} step {step}: a full scan found what the index did not: {full:?}"
            );
            let walked = status_of(&daemon, &ws, true).await;
            assert_eq!(
                index.working_copy, walked.working_copy,
                "seed {seed} step {step}"
            );
            assert_eq!(changed(&index), changed(&walked), "seed {seed} step {step}");
            assert_eq!(index.conflicted_paths, walked.conflicted_paths);
        }
        assert_eq!(journals.misses(), 0, "seed {seed}");
    }
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn overflow_walks_then_recovers() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let other = create(&daemon, &repo).await;
    settle(&daemon, &ws).await;
    let scheduler = daemon.handle.state().manager.scheduler();
    // No watcher snapshot of the injected event: one that took the
    // journal first would leave the read nothing lost to walk for.
    let _paused = scheduler.pause(id_of(&ws));
    scheduler.inject_event(id_of(&ws), Vec::new(), true);
    let status = status_of(&daemon, &ws, false).await;
    assert_eq!(
        source(&status),
        walk(StatusWalkReason::EventsLost),
        "{status:?}"
    );
    assert_eq!(source(&status_of(&daemon, &ws, false).await), INDEX);
    assert_eq!(source(&status_of(&daemon, &other, false).await), INDEX);
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn restart_walks_once() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    settle(&daemon, &ws).await;
    let leftover = sync_dir(&daemon, &repo, &ws).join("1-1");
    std::fs::write(&leftover, "").unwrap();
    let orphan = daemon
        .config
        .paths
        .repos
        .join(&repo.id)
        .join("sync")
        .join("00000000000000ff");
    std::fs::create_dir_all(&orphan).unwrap();
    let daemon = daemon.restart().await;
    assert!(!leftover.exists() && !orphan.exists(), "swept at start");
    write(&ws, "after-restart.txt", "r\n");
    let status = status_of(&daemon, &ws, false).await;
    assert_eq!(
        source(&status),
        walk(StatusWalkReason::WatchRestarted),
        "{status:?}"
    );
    assert!(changed(&status).contains("after-restart.txt"));
    assert_eq!(source(&status_of(&daemon, &ws, false).await), INDEX);
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn unwatched_workspace_walks() {
    let (daemon, repo) = fixture().await;
    daemon.handle.state().manager.scheduler().refuse_watches();
    let ws = create(&daemon, &repo).await;
    write(&ws, "unwatched.txt", "u\n");
    let status = status_of(&daemon, &ws, false).await;
    assert_eq!(
        source(&status),
        walk(StatusWalkReason::NotWatched),
        "{status:?}"
    );
    assert!(changed(&status).contains("unwatched.txt"));
}

// 6, Linux: a create's first status before its background attach is up.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn attaching_workspace_walks() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.watcher.notify_attach_background = true;
    })
    .await;
    let _hold = daemon.handle.state().manager.scheduler().hold_attaches();
    let ws = create(&daemon, &repo).await;
    let status = status_of(&daemon, &ws, false).await;
    assert_eq!(
        source(&status),
        walk(StatusWalkReason::NotWatched),
        "{status:?}"
    );
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn sync_timeout_walks() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.status_index.sync_timeout = Duration::from_millis(100);
    })
    .await;
    let ws = create(&daemon, &repo).await;
    settle(&daemon, &ws).await;
    let journals = daemon.handle.state().manager.scheduler().journals();
    journals.swallow_cookies(true);
    write(&ws, "unsynced.txt", "n\n");
    let status = status_of(&daemon, &ws, false).await;
    assert_eq!(
        source(&status),
        walk(StatusWalkReason::NotSynced),
        "{status:?}"
    );
    assert!(changed(&status).contains("unsynced.txt"));
    let sync_dir = sync_dir(&daemon, &repo, &ws);
    assert_eq!(
        std::fs::read_dir(&sync_dir).unwrap().count(),
        0,
        "no cookie left in {}",
        sync_dir.display()
    );
}

fn sync_dir(daemon: &TestDaemon, repo: &SyntheticRepo, ws: &Workspace) -> PathBuf {
    daemon
        .config
        .paths
        .repos
        .join(&repo.id)
        .join("sync")
        .join(&ws.id)
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn too_broad_walks() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.watcher.narrowed_max_paths = 5;
    })
    .await;
    let ws = create(&daemon, &repo).await;
    settle(&daemon, &ws).await;
    for n in 0..6 {
        write(&ws, &format!("broad-{n}.txt"), "b\n");
    }
    let status = status_of(&daemon, &ws, false).await;
    assert_eq!(
        source(&status),
        walk(StatusWalkReason::TooBroad),
        "{status:?}"
    );
    settle(&daemon, &ws).await;
    write(&ws, ".reefignore", "*.tmp\n");
    let status = status_of(&daemon, &ws, false).await;
    assert_eq!(
        source(&status),
        walk(StatusWalkReason::TooBroad),
        "{status:?}"
    );
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn failed_snapshot_marks_the_index_stale() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    settle(&daemon, &ws).await;
    let scheduler = daemon.handle.state().manager.scheduler();
    scheduler.fail_scans(true);
    write(&ws, "lost-by-a-failure.txt", "f\n");
    let failed = daemon
        .client
        .get_status(GetStatusRequest {
            workspace: ws.id.clone(),
            ..GetStatusRequest::default()
        })
        .await;
    assert!(failed.is_err(), "{failed:?}");
    scheduler.fail_scans(false);
    let status = status_of(&daemon, &ws, false).await;
    assert_eq!(
        source(&status),
        walk(StatusWalkReason::SnapshotFailed),
        "{status:?}"
    );
    assert!(changed(&status).contains("lost-by-a-failure.txt"));
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn covered_reads() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    settle(&daemon, &ws).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    write(&ws, "concurrent.txt", "c\n");
    let reads = (0..20).map(|_| status_of(&daemon, &ws, false));
    let statuses = futures::future::join_all(reads).await;
    for status in &statuses {
        assert!(changed(status).contains("concurrent.txt"), "{status:?}");
        assert_eq!(source(status), INDEX, "{status:?}");
    }
    let explicit = outcomes
        .all()
        .into_iter()
        .filter(|outcome| outcome.kind == SnapshotKind::Explicit)
        .count();
    assert!(explicit <= 2, "{:#?}", outcomes.all());
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn index_disabled_is_r2_02() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.status_index.enabled = false;
        config.file.status_index.sync_timeout = Duration::from_mins(1);
    })
    .await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    // A read that synced would wait the minute out.
    let journals = daemon.handle.state().manager.scheduler().journals();
    journals.swallow_cookies(true);
    write(&ws, "plain.txt", "p\n");
    let status = tokio::time::timeout(BOUND, status_of(&daemon, &ws, false))
        .await
        .expect("no sync");
    assert_eq!(status.source(), StatusSource::Snapshot, "{status:?}");
    assert_eq!(status.walk_reason(), StatusWalkReason::Unspecified);
    let explicit: Vec<_> = outcomes
        .all()
        .into_iter()
        .filter(|outcome| outcome.kind == SnapshotKind::Explicit)
        .collect();
    assert!(
        explicit.iter().all(|outcome| !outcome.narrowed),
        "{explicit:#?}"
    );
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn skip_snapshot_and_at_operation_do_not_sync() {
    let (daemon, repo) = fixture_with(|config| {
        // A read that synced would wait the minute out.
        config.file.status_index.sync_timeout = Duration::from_mins(1);
    })
    .await;
    let ws = create(&daemon, &repo).await;
    settle(&daemon, &ws).await;
    let journals = daemon.handle.state().manager.scheduler().journals();
    // Every sync would time out: these reads must not wait for one.
    journals.swallow_cookies(true);
    let outcomes = Outcomes::record(&daemon, &ws);
    let skipped = tokio::time::timeout(
        BOUND,
        daemon.client.get_status(GetStatusRequest {
            workspace: ws.id.clone(),
            skip_snapshot: true,
            ..GetStatusRequest::default()
        }),
    )
    .await
    .expect("no sync")
    .unwrap();
    assert_eq!(
        skipped.workspace_status.unwrap().source(),
        StatusSource::LastSnapshot
    );
    let at = tokio::time::timeout(
        BOUND,
        daemon.client.get_status(GetStatusRequest {
            workspace: ws.id.clone(),
            at_operation: skipped.at_operation,
            ..GetStatusRequest::default()
        }),
    )
    .await
    .expect("no sync")
    .unwrap();
    assert_eq!(
        at.workspace_status.unwrap().source(),
        StatusSource::Operation
    );
    assert_eq!(outcomes.len(), 0, "no scan");
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn missed_change_is_reported() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    hidden_dir(&daemon, &ws).await;
    let journals = daemon.handle.state().manager.scheduler().journals();
    let hidden = std::fs::canonicalize(&ws.path).unwrap().join("hidden");
    journals.drop_events_under(hidden);
    write(&ws, "hidden/unseen.txt", "u\n");
    let status = status_of(&daemon, &ws, false).await;
    assert!(
        !changed(&status).contains("hidden/unseen.txt"),
        "the stated gap: no event named it"
    );
    let full = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(full.operation.is_some(), "the full scan records it");
    assert_eq!(journals.misses(), 1);
    let status = status_of(&daemon, &ws, true).await;
    assert!(changed(&status).contains("hidden/unseen.txt"));
}

// 15
#[tokio::test(flavor = "multi_thread")]
async fn no_events_lost_across_creates() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    settle(&daemon, &ws).await;
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let writer = {
        let (ws, stop) = (ws.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut written = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                let name = format!("burst-{}.txt", written.len());
                write(&ws, &name, "b\n");
                written.push(name);
                std::thread::sleep(Duration::from_millis(2));
            }
            written
        })
    };
    for _ in 0..20 {
        let other = create(&daemon, &repo).await;
        daemon
            .client
            .release_workspace(&repo.id, &other.id, false, false)
            .await
            .unwrap();
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let written = writer.join().unwrap();
    let status = status_of(&daemon, &ws, false).await;
    assert_eq!(source(&status), INDEX, "{status:?}");
    // Status lists at most 1,000 files; the count covers them all.
    assert_eq!(
        status.changed_file_count as usize,
        written.len(),
        "every burst file is in the index's answer"
    );
    daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let journals = daemon.handle.state().manager.scheduler().journals();
    assert_eq!(journals.misses(), 0);
}

// 16
#[tokio::test(flavor = "multi_thread")]
async fn cookies_order_after_writes() {
    let (daemon, repo) = fixture().await;
    let (a, b, c) = (
        create(&daemon, &repo).await,
        create(&daemon, &repo).await,
        create(&daemon, &repo).await,
    );
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let others = {
        let (b, c, stop) = (b.clone(), c.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            let mut n = 0;
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                write(&b, &format!("b-{}.txt", n % 50), &n.to_string());
                write(&c, &format!("c-{}.txt", n % 50), &n.to_string());
                n += 1;
            }
        })
    };
    let rounds = std::env::var("REEF_INDEX_ROUNDS")
        .ok()
        .and_then(|rounds| rounds.parse().ok())
        .unwrap_or(150);
    for round in 0..rounds {
        let name = format!("a-{round}.txt");
        write(&a, &name, "a\n");
        let status = status_of(&daemon, &a, false).await;
        assert!(
            changed(&status).contains(&name),
            "round {round}: {status:?}"
        );
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    others.join().unwrap();
}

// 17
#[tokio::test(flavor = "multi_thread")]
async fn watcher_snapshot_skips_an_empty_journal() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    settle(&daemon, &ws).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    write(&ws, "taken-by-a-read.txt", "t\n");
    let status = status_of(&daemon, &ws, false).await;
    assert!(changed(&status).contains("taken-by-a-read.txt"));
    // Past the debounce and `min_interval`: the event's watcher snapshot
    // would have run by now.
    tokio::time::sleep(Duration::from_millis(1_000)).await;
    let watcher: Vec<_> = outcomes
        .all()
        .into_iter()
        .filter(|outcome| outcome.kind == SnapshotKind::Watcher)
        .collect();
    assert!(watcher.is_empty(), "{watcher:#?}");
}

// 18
#[tokio::test(flavor = "multi_thread")]
async fn cookies_never_enter_the_root() {
    use notify::Watcher as _;

    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    settle(&daemon, &ws).await;
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let mut watcher = notify::recommended_watcher(move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event
            && !matches!(event.kind, notify::EventKind::Access(_))
        {
            sink.lock().unwrap().extend(event.paths);
        }
    })
    .unwrap();
    watcher
        .watch(Path::new(&ws.path), notify::RecursiveMode::Recursive)
        .unwrap();
    for _ in 0..100 {
        status_of(&daemon, &ws, false).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(*seen.lock().unwrap(), Vec::<PathBuf>::new());
    drop(watcher);

    let dir = sync_dir(&daemon, &repo, &ws);
    assert!(dir.is_dir());
    daemon
        .client
        .release_workspace(&repo.id, &ws.id, false, false)
        .await
        .unwrap();
    assert!(!dir.exists(), "removed at release");
}

// 19
#[tokio::test(flavor = "multi_thread")]
async fn busy_workspace_is_fully_scanned_on_the_timer() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.watcher.fallback_interval = Duration::from_secs(1);
        config.file.watcher.watched_fallback_interval = Duration::from_secs(3);
    })
    .await;
    let ws = create(&daemon, &repo).await;
    hidden_dir(&daemon, &ws).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let journals = daemon.handle.state().manager.scheduler().journals();
    journals.drop_events_under(std::fs::canonicalize(&ws.path).unwrap().join("hidden"));
    write(&ws, "hidden/unseen.txt", "u\n");
    let mut n = 0;
    // Busy: a write every 300 ms, so the idle fallback never fires.
    until("a timer scan records the unseen write", async || {
        write(&ws, &format!("busy-{}.txt", n % 5), &n.to_string());
        n += 1;
        tokio::time::sleep(Duration::from_millis(300)).await;
        status_of(&daemon, &ws, true)
            .await
            .changed_files
            .iter()
            .any(|file| file.path == "hidden/unseen.txt")
    })
    .await;
    // The hooks run just after the commit a status can already see.
    until("the timer's full scan is reported", async || {
        outcomes
            .all()
            .iter()
            .any(|outcome| outcome.kind == SnapshotKind::Watcher && !outcome.narrowed)
    })
    .await;
    assert!(journals.misses() >= 1);
}

// 20, Linux: a write through a shared mapping is seen at its close.
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn mmap_write_is_seen_at_close() {
    use std::num::NonZeroUsize;

    use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap};

    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    write(&ws, "mapped.bin", &"0".repeat(4096));
    settle(&daemon, &ws).await;
    let path = Path::new(&ws.path).join("mapped.bin");
    {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let len = NonZeroUsize::new(4096).unwrap();
        // SAFETY: a fresh shared mapping of a file this test owns, written
        // within its length and unmapped before the file closes.
        #[allow(unsafe_code, reason = "mmap(2): the write this test is about")]
        unsafe {
            let map = mmap(
                None,
                len,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_SHARED,
                &file,
                0,
            )
            .unwrap();
            std::ptr::write_bytes(map.as_ptr().cast::<u8>(), b'1', 4096);
            munmap(map, 4096).unwrap();
        }
    }
    let status = status_of(&daemon, &ws, false).await;
    assert_eq!(source(&status), INDEX, "{status:?}");
    let full = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(full.operation.is_none(), "{full:?}");
}
