//! Narrowed notify snapshots (R1.30, ADR 0026): a watcher snapshot of a
//! notify workspace scans only the paths reported since the last one, and
//! everything under them. Numbered as the brief numbers them; 1 and 2 are
//! in `snapshots.rs` and `watcher/notify.rs`, 15 is reef-lib's
//! `in_memory_working_copy_follows_a_narrowed_snapshot`.
//!
//! Both fallback timers are an hour here, so no fallback snapshot can stand
//! in for the scan under test.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use common::{SyntheticRepo, TestDaemon, test_settings};
use reef_api::v1::{CreateWorkspaceRequest, Workspace};
use reef_daemon::model::WorkspaceId;
use reef_daemon::snapshot::{SnapshotKind, SnapshotOutcome};
use reef_daemon::workspace::SnapshotSpec;
use reef_lib::jj::{CommitIdHex, Repo, RepoPath};
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";
/// How long a write may take to reach a snapshot: debug builds attach
/// `FSEvents` watches slowly.
const BOUND: Duration = Duration::from_secs(20);

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
        // A Linux background attach (R1.32) ends in a full scan, which would
        // take the first write after a create; these tests are about the
        // scans after it.
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

/// The workspace's root as the watchers report it.
fn canonical(ws: &Workspace) -> PathBuf {
    std::fs::canonicalize(&ws.path).unwrap()
}

/// A tracked file of the synthetic tree, relative to the root.
fn tracked_file(ws: &Workspace) -> String {
    fn find(dir: &Path, root: &Path) -> Option<PathBuf> {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let name = path.file_name().unwrap().to_string_lossy();
            if name.starts_with('.') {
                continue;
            }
            if path.is_file() {
                return Some(path.strip_prefix(root).unwrap().to_owned());
            }
            if path.is_dir()
                && let Some(found) = find(&path, root)
            {
                return Some(found);
            }
        }
        None
    }
    let root = Path::new(&ws.path);
    find(root, root)
        .expect("the synthetic tree has a file")
        .to_string_lossy()
        .replace('\\', "/")
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

/// `path` in `commit`'s tree, as text.
async fn file_at(root: &Path, commit: &CommitIdHex, path: &str) -> Option<String> {
    let repo = open(root).await;
    let (commit, path) = (commit.clone(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let path = RepoPath::from_internal_string(&path).unwrap();
        pollster::block_on(repo.read_file_at(&commit, path))
            .unwrap()
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
    })
    .await
    .unwrap()
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

    fn len(&self) -> usize {
        self.0.lock().unwrap().len()
    }

    fn all(&self) -> Vec<SnapshotOutcome> {
        self.0.lock().unwrap().clone()
    }

    /// Waits for the first outcome after the first `after` for which
    /// `check` holds.
    async fn wait<F, Fut>(&self, after: usize, what: &str, mut check: F) -> SnapshotOutcome
    where
        F: FnMut(SnapshotOutcome) -> Fut,
        Fut: Future<Output = bool>,
    {
        let begun = Instant::now();
        let mut seen = after;
        loop {
            let fresh: Vec<_> = self.0.lock().unwrap()[seen..].to_vec();
            seen += fresh.len();
            for outcome in fresh {
                if check(outcome.clone()).await {
                    return outcome;
                }
            }
            assert!(
                begun.elapsed() < BOUND,
                "{what}: not within {BOUND:?}; outcomes {:#?}",
                self.all()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Waits for a watcher snapshot after the first `after` whose commit
    /// has `path` with `contents` (`None`: without `path`).
    async fn holding(
        &self,
        repo: &SyntheticRepo,
        after: usize,
        path: &str,
        contents: Option<&str>,
    ) -> SnapshotOutcome {
        let what = format!("a watcher snapshot with {path} = {contents:?}");
        self.wait(after, &what, |outcome| async move {
            outcome.kind == SnapshotKind::Watcher
                && file_at(&repo.root, &outcome.commit_id, path)
                    .await
                    .as_deref()
                    == contents
        })
        .await
    }
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn narrowed_scan_records_the_write() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let path = tracked_file(&ws);
    write(&ws, &path, "rewritten\n");
    let outcome = outcomes.holding(&repo, 0, &path, Some("rewritten\n")).await;
    assert!(outcome.narrowed, "{outcome:?}");
    assert!(outcome.paths_count >= 1, "{outcome:?}");
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn directory_moved_in_is_snapshotted() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let outside = staging(&repo, &ws, &outcomes).await.join("batch");
    std::fs::create_dir_all(outside.join("sub")).unwrap();
    for i in 0..50 {
        std::fs::write(outside.join("sub").join(format!("{i}.txt")), "in\n").unwrap();
    }
    let before = outcomes.len();
    std::fs::rename(&outside, Path::new(&ws.path).join("arrived")).unwrap();
    let outcome = outcomes
        .holding(&repo, before, "arrived/sub/49.txt", Some("in\n"))
        .await;
    assert!(outcome.narrowed, "{outcome:?}");
    for i in 0..50 {
        let path = format!("arrived/sub/{i}.txt");
        assert_eq!(
            file_at(&repo.root, &outcome.commit_id, &path)
                .await
                .as_deref(),
            Some("in\n"),
            "{path}"
        );
    }
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn directory_moved_out_is_deleted() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let outside = staging(&repo, &ws, &outcomes).await.join("leaving");
    for i in 0..20 {
        write(&ws, &format!("leaving/deep/{i}.txt"), "x\n");
    }
    outcomes
        .holding(&repo, 0, "leaving/deep/19.txt", Some("x\n"))
        .await;
    let before = outcomes.len();
    std::fs::rename(Path::new(&ws.path).join("leaving"), &outside).unwrap();
    let outcome = outcomes
        .holding(&repo, before, "leaving/deep/0.txt", None)
        .await;
    assert!(outcome.narrowed, "{outcome:?}");
    for i in 0..20 {
        let path = format!("leaving/deep/{i}.txt");
        assert_eq!(
            file_at(&repo.root, &outcome.commit_id, &path).await,
            None,
            "{path}"
        );
    }
}

/// `staging/`, a directory the workspace's snapshots ignore, once a
/// snapshot has recorded the `.reefignore` that says so. To a snapshot it
/// is outside the workspace; a directory from elsewhere could not be
/// renamed in on Linux, where a workspace is its own overlay mount
/// (`EXDEV`). inotify names a directory moved out of it, not the files
/// inside, as it would from outside.
async fn staging(repo: &SyntheticRepo, ws: &Workspace, outcomes: &Outcomes) -> PathBuf {
    write(ws, ".reefignore", "staging/\n");
    let staging = Path::new(&ws.path).join("staging");
    std::fs::create_dir(&staging).unwrap();
    outcomes
        .holding(repo, 0, ".reefignore", Some("staging/\n"))
        .await;
    staging
}

// 6: inotify names the new directory, not the files written into it before
// its watch exists. Run it in a loop on Linux:
// `for i in $(seq 20); do cargo nextest run -p reef-daemon --test narrowed -E 'test(new_directory)' || break; done`
#[tokio::test(flavor = "multi_thread")]
async fn files_written_into_a_new_directory_at_once() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let dir = Path::new(&ws.path).join("fresh");
    std::fs::create_dir(&dir).unwrap();
    for i in 0..20 {
        std::fs::write(dir.join(format!("{i}.txt")), "new\n").unwrap();
    }
    let outcome = outcomes
        .holding(&repo, 0, "fresh/19.txt", Some("new\n"))
        .await;
    for i in 0..20 {
        let path = format!("fresh/{i}.txt");
        assert_eq!(
            file_at(&repo.root, &outcome.commit_id, &path)
                .await
                .as_deref(),
            Some("new\n"),
            "{path}"
        );
    }
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn everything_scans_everything() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let scheduler = daemon.handle.state().manager.scheduler();
    scheduler.pause_watcher();
    write(&ws, "unreported.txt", "lost\n");
    // Past the debounce, so the write's event is dropped, not delivered late.
    tokio::time::sleep(Duration::from_secs(1)).await;
    scheduler.resume_watcher();
    scheduler.inject_event(id_of(&ws), Vec::new(), true);
    let outcome = outcomes
        .holding(&repo, 0, "unreported.txt", Some("lost\n"))
        .await;
    assert!(!outcome.narrowed, "{outcome:?}");
    assert_eq!(outcome.paths_count, 0, "{outcome:?}");
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn too_many_paths_scans_everything() {
    let (daemon, repo) = fixture_with(|config| config.file.watcher.narrowed_max_paths = 10).await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let scheduler = daemon.handle.state().manager.scheduler();
    // All eleven are reported before the scan takes the set. At the root:
    // in a new directory, one reported path would cover them all.
    let hold = scheduler.hold_scans().await;
    for i in 0..11 {
        write(&ws, &format!("many-{i}.txt"), "m\n");
    }
    tokio::time::sleep(Duration::from_secs(2)).await;
    drop(hold);
    let outcome = outcomes.holding(&repo, 0, "many-10.txt", Some("m\n")).await;
    assert!(!outcome.narrowed, "{outcome:?}");
    for i in 0..11 {
        let path = format!("many-{i}.txt");
        assert!(
            file_at(&repo.root, &outcome.commit_id, &path)
                .await
                .is_some(),
            "{path}"
        );
    }
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn reefignore_change_scans_everything() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    write(&ws, ".reefignore", "hidden/\n");
    write(&ws, "hidden/secret.txt", "s\n");
    write(&ws, "sub/.gitignore", "local.txt\n");
    write(&ws, "sub/local.txt", "l\n");
    outcomes
        .holding(&repo, 0, "sub/.gitignore", Some("local.txt\n"))
        .await;
    let before = outcomes.len();

    // Reef's own ignore file: jj does not rescan for it. Wait for the
    // snapshot that has the emptied file, not the first with the secret: the
    // setup's events after `.reefignore`'s can come in a later debouncer
    // batch, after the first snapshot took its set, and that batch's
    // narrowed scan, reading the new rules, records the secret before the
    // `.reefignore` event arrives (fuse-overlayfs on CI).
    write(&ws, ".reefignore", "");
    let outcome = outcomes
        .holding(&repo, before, ".reefignore", Some(""))
        .await;
    assert!(!outcome.narrowed, "{outcome:?}");
    assert_eq!(
        file_at(&repo.root, &outcome.commit_id, "hidden/secret.txt")
            .await
            .as_deref(),
        Some("s\n"),
        "{outcome:?}"
    );
    let before = outcomes.len();

    // A `.gitignore`: jj rescans its directory, narrowed.
    write(&ws, "sub/.gitignore", "");
    let outcome = outcomes
        .holding(&repo, before, "sub/local.txt", Some("l\n"))
        .await;
    assert!(outcome.narrowed, "{outcome:?}");
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn events_while_waiting_and_scanning_are_kept() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let scheduler = daemon.handle.state().manager.scheduler();

    // B arrives while a snapshot waits for its scan: it joins that
    // snapshot's set, with A.
    let hold = scheduler.hold_scans().await;
    write(&ws, "a.txt", "a\n");
    wait_until("a snapshot at the hold", || scheduler.held_scans() > 0).await;
    write(&ws, "b.txt", "b\n");
    tokio::time::sleep(Duration::from_secs(1)).await;
    drop(hold);
    let outcome = outcomes.holding(&repo, 0, "a.txt", Some("a\n")).await;
    assert!(outcome.narrowed, "{outcome:?}");
    assert_eq!(
        file_at(&repo.root, &outcome.commit_id, "b.txt")
            .await
            .as_deref(),
        Some("b\n"),
        "B joined A's snapshot: {outcome:?}"
    );

    // D arrives while C's snapshot scans: it is in that one or the next.
    let before = outcomes.len();
    let hold = scheduler.hold_scans().await;
    write(&ws, "c.txt", "c\n");
    wait_until("a snapshot at the hold", || scheduler.held_scans() > 0).await;
    drop(hold);
    write(&ws, "d.txt", "d\n");
    let outcome = outcomes.holding(&repo, before, "d.txt", Some("d\n")).await;
    assert_eq!(
        file_at(&repo.root, &outcome.commit_id, "c.txt")
            .await
            .as_deref(),
        Some("c\n")
    );
    assert!(
        outcomes.all()[before..]
            .iter()
            .all(|outcome| outcome.kind == SnapshotKind::Watcher),
        "no explicit or fallback snapshot stood in"
    );
}

/// Polls `check` every 10 ms until it holds, failing after [`BOUND`].
async fn wait_until(what: &str, mut check: impl FnMut() -> bool) {
    let begun = Instant::now();
    while !check() {
        assert!(begun.elapsed() < BOUND, "{what}: not within {BOUND:?}");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

// 11: a store mount is a symlink jj never follows, and the scan ignores it.
#[tokio::test(flavor = "multi_thread")]
async fn store_mount_is_not_walked() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let manager = &daemon.handle.state().manager;
    let scheduler = manager.scheduler();
    let id = id_of(&ws);
    scheduler.pause_watcher();
    let store = tempfile::tempdir().unwrap();
    for i in 0..10_000 {
        std::fs::write(store.path().join(format!("{i}.js")), "s\n").unwrap();
    }
    std::fs::create_dir(Path::new(&ws.path).join("vendor")).unwrap();
    std::os::unix::fs::symlink(store.path(), Path::new(&ws.path).join("vendor/deps")).unwrap();
    write(&ws, "vendor/README", "r\n");
    manager
        .set_snapshot_spec(
            id,
            SnapshotSpec {
                excludes: vec![],
                mounts: vec!["vendor/deps".to_owned()],
            },
        )
        .await
        .unwrap();
    // The rewatch leaves the workspace `behind` until a watcher snapshot,
    // which is full.
    scheduler.inject_event(id, vec![canonical(&ws).join("vendor/README")], false);
    let first = outcomes
        .holding(&repo, 0, "vendor/README", Some("r\n"))
        .await;
    assert!(!first.narrowed, "{first:?}");

    let full = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(!full.changed);
    let full = outcomes
        .wait(0, "the explicit snapshot", |outcome| async move {
            outcome.kind == SnapshotKind::Explicit
        })
        .await;

    // Like the full scan, one with nothing to record: a walk that followed
    // the mount would find the store's 10,000 files.
    let before = outcomes.len();
    scheduler.inject_event(id, vec![canonical(&ws).join("vendor")], false);
    let narrowed = outcomes
        .wait(before, "the narrowed snapshot", |outcome| async move {
            outcome.kind == SnapshotKind::Watcher
        })
        .await;
    assert!(narrowed.narrowed, "{narrowed:?}");
    assert!(!narrowed.changed, "{narrowed:?}");
    for path in ["vendor/deps", "vendor/deps/0.js"] {
        assert_eq!(
            file_at(&repo.root, &narrowed.commit_id, path).await,
            None,
            "{path}"
        );
    }
    // A tenth of the full scan's walk, or 5 ms where that rounds to less.
    // With debug assertions jj checks every scan's whole tree against its
    // file states, which no narrowing removes: there, about the full scan,
    // with half again for a loaded runner (macos-15 measured 22 ms against
    // 20 for the workspace's 1,000 files). jj never walks through the
    // mount's symlink, and a walk that reached the store would record its
    // files, so `changed` and the paths above are the real check; this
    // bound only flags a walk grown far slower.
    let bound = if cfg!(debug_assertions) {
        let full = full.walk_ms.max(5);
        full.saturating_add(full / 2)
    } else {
        full.walk_ms.max(50) / 10
    };
    assert!(
        narrowed.walk_ms <= bound,
        "narrowed walk {} ms, full {} ms",
        narrowed.walk_ms,
        full.walk_ms
    );
}

// 12
#[cfg(target_os = "macos")]
#[tokio::test(flavor = "multi_thread")]
async fn case_insensitive_write_is_snapshotted() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    write(&ws, "README.md", "one\n");
    outcomes.holding(&repo, 0, "README.md", Some("one\n")).await;
    if !Path::new(&ws.path).join("readme.md").exists() {
        eprintln!("skipped: the workspace's volume is case-sensitive");
        return;
    }
    let before = outcomes.len();
    write(&ws, "readme.md", "two\n");
    let outcome = outcomes
        .holding(&repo, before, "README.md", Some("two\n"))
        .await;
    assert!(outcome.narrowed, "{outcome:?}");
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn unwatched_workspace_is_not_narrowed() {
    let (daemon, repo) = fixture().await;
    let scheduler = daemon.handle.state().manager.scheduler();
    scheduler.refuse_watches();
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    write(&ws, "unwatched.txt", "u\n");
    scheduler.inject_event(
        id_of(&ws),
        vec![canonical(&ws).join("unwatched.txt")],
        false,
    );
    let outcome = outcomes
        .holding(&repo, 0, "unwatched.txt", Some("u\n"))
        .await;
    assert!(!outcome.narrowed, "{outcome:?}");
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn explicit_and_final_snapshots_are_full() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let outcomes = Outcomes::record(&daemon, &ws);
    let scheduler = daemon.handle.state().manager.scheduler();
    let id = id_of(&ws);
    scheduler.pause_watcher();
    // Each snapshot below has a reported path waiting, which no scheduled
    // snapshot takes first.
    let _hold = scheduler.pause(id);
    let reported = |path: &str| vec![canonical(&ws).join(path)];

    write(&ws, "explicit.txt", "e\n");
    scheduler.inject_event(id, reported("explicit.txt"), false);
    daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    write(&ws, "inspected.txt", "i\n");
    scheduler.inject_event(id, reported("inspected.txt"), false);
    daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, true)
        .await
        .unwrap();
    write(&ws, "final.txt", "f\n");
    scheduler.inject_event(id, reported("final.txt"), false);
    daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    let all = outcomes.all();
    assert_eq!(all.len(), 3, "{all:#?}");
    assert!(all.iter().all(|outcome| !outcome.narrowed), "{all:#?}");
    assert!(all.iter().all(|outcome| outcome.changed), "{all:#?}");
    assert_eq!(all[2].kind, SnapshotKind::Final, "{all:#?}");
}
