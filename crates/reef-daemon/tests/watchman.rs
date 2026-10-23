//! The Watchman watcher end to end (R1.14), numbered as the brief numbers
//! them. Every test skips, saying so, when `watchman` is not on `PATH`.
//!
//! The tests share the machine's one Watchman server, and test 4 shuts it
//! down, so they run one at a time: a lock here for `cargo test`, and the
//! `watchman` test group in `.config/nextest.toml` for nextest.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::path::Path;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use common::{SyntheticRepo, TestDaemon, test_settings};
use jj_lib::backend::CommitId;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::{Config, WatcherBackend};
use reef_daemon::model::WorkspaceId;
use reef_daemon::proto::v0::register_request::IfNotJj;
use reef_daemon::proto::v0::{CreateRequest, ListWorkspacesRequest, Workspace};
use reef_daemon::watcher::watchman::WatchmanWatcher;
use reef_daemon::workspace::SnapshotSpec;
use reef_lib::error::ErrorKind;
use reef_lib::jj::Repo;
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";
const FALLBACK: Duration = Duration::from_secs(2);

/// Returns from the test, with a visible message, without `watchman`.
macro_rules! require_watchman {
    () => {
        if !WatchmanWatcher::binary_on_path() {
            eprintln!("skipped: watchman not on PATH");
            return;
        }
    };
}

/// Held for a whole test: see the module docs.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Everything the daemons in this process logged, at `info` and above.
static LOGS: Mutex<Vec<u8>> = Mutex::new(Vec::new());

struct Capture;

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOGS.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(buf);
        if std::env::var_os("TEST_LOG").is_some() {
            std::io::stderr().write_all(buf)?;
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Log lines that contain `message` and mention `workspace`.
fn logged(message: &str, workspace: &str) -> usize {
    let logs = LOGS.lock().unwrap_or_else(PoisonError::into_inner);
    String::from_utf8_lossy(&logs)
        .lines()
        .filter(|line| line.contains(message) && line.contains(workspace))
        .count()
}

/// A daemon with R1.13's test intervals, `watcher.backend = "watchman"`
/// unless `configure` changes it, and a `preset` repository registered.
/// Subscriptions are not deferred (R1.29) unless `configure` sets
/// `watchman_attach_quiet` again.
async fn fixture(
    preset: Preset,
    configure: impl FnOnce(&mut Config),
) -> (TestDaemon, SyntheticRepo) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_ansi(false)
        .with_writer(|| Capture)
        .try_init();
    TestDaemon::with_synthetic_repo_and(preset, |config| {
        let watcher = &mut config.file.watcher;
        watcher.backend = WatcherBackend::Watchman;
        watcher.debounce = Duration::from_millis(100);
        watcher.min_interval = Duration::from_millis(100);
        watcher.fallback_interval = FALLBACK;
        // Tests that wait for a fallback snapshot of a subscribed workspace
        // need not wait the default ten minutes.
        watcher.watched_fallback_interval = FALLBACK;
        watcher.watchman_attach_quiet = Duration::ZERO;
        configure(config);
    })
    .await
}

async fn create(daemon: &TestDaemon, repo: &SyntheticRepo) -> Workspace {
    daemon
        .client
        .create_workspace(CreateRequest {
            repo: repo.id.clone(),
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

/// Whether `path` is in `commit`'s tree.
async fn tree_has(root: &Path, commit: &str, path: &str) -> bool {
    let (root, commit, path) = (root.to_owned(), commit.to_owned(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let repo = pollster::block_on(Repo::open(&root, &test_settings())).unwrap();
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

/// Waits until `ws` has `count` subscriptions established.
async fn subscribed(ws: &Workspace, count: usize) {
    wait_for(
        Duration::from_secs(10),
        "the watchman subscription",
        || async { logged("watchman subscription established", &ws.id) >= count },
    )
    .await;
}

/// Waits until `ws` has a snapshot with `path` in it, failing after `bound`.
async fn snapshotted(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    ws: &Workspace,
    path: &str,
    bound: Duration,
) -> Duration {
    wait_for(bound, &format!("a snapshot with {path}"), || async {
        let row = inspect(daemon, repo, ws).await;
        !row.last_snapshot_op.is_empty() && tree_has(&repo.root, &row.commit_id, path).await
    })
    .await
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn watchman_triggers_snapshot() {
    require_watchman!();
    let _serial = SERIAL.lock().await;
    let (daemon, repo) = fixture(Preset::Small, |_| {}).await;
    let ws = create(&daemon, &repo).await;
    assert_eq!(ws.watcher, "watchman");
    assert_eq!(inspect(&daemon, &repo, &ws).await.watcher, "watchman");
    let listed = daemon
        .client
        .list_workspaces(ListWorkspacesRequest {
            repo: repo.id.clone(),
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(listed[0].watcher, "watchman");
    subscribed(&ws, 1).await;
    assert_eq!(inspect(&daemon, &repo, &ws).await.attention, "");

    write(&ws, "agent-wrote-this.txt", "hello\n");
    let waited = snapshotted(
        &daemon,
        &repo,
        &ws,
        "agent-wrote-this.txt",
        Duration::from_secs(3),
    )
    .await;
    eprintln!("write reached the operation log in {waited:.2?}");
}

// 2. Manual: `cargo test --release -p reef-daemon --test watchman --
// --ignored scan_is_narrowed --nocapture`. Numbers in the PR and
// docs/benchmarks.md. It runs the binary, which sets `WATCHMAN_SOCK` at
// start as a real daemon does. The 3x bound is checked on `large`: on
// `medium` a full scan is already about as short as Watchman's per-query
// cost (ADR 0023).
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a measurement; CI runners are too noisy for its bound"]
async fn scan_is_narrowed() {
    require_watchman!();
    let _serial = SERIAL.lock().await;
    for preset in [Preset::Medium, Preset::Large] {
        let notify = scan_ms_after_one_change(preset, WatcherBackend::Notify).await;
        let watchman = scan_ms_after_one_change(preset, WatcherBackend::Watchman).await;
        eprintln!("{preset:?}: notify scan_ms {notify:?}, watchman scan_ms {watchman:?}");
        let (notify, watchman) = (notify[notify.len() / 2], watchman[watchman.len() / 2]);
        eprintln!(
            "{preset:?}, scan after one changed file (median of 9): notify {notify} ms, \
             watchman {watchman} ms"
        );
        if preset == Preset::Large {
            assert!(
                notify >= 3 * watchman.max(1),
                "the Watchman scan ({watchman} ms) is not 3x shorter than the full one ({notify} ms)"
            );
        }
    }
}

/// The `scan_ms` of nine explicit snapshots after one changed file each,
/// sorted, on a `preset` repository, from a `reef-daemon` binary with
/// `backend` and no automatic snapshots.
async fn scan_ms_after_one_change(preset: Preset, backend: WatcherBackend) -> Vec<u64> {
    let tmp = common::short_tempdir();
    let state = tmp.path().join("state");
    let config = tmp.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[watcher]\nbackend = \"{}\"\ndebounce = \"1h\"\nmin_interval = \"1h\"\n\
             fallback_interval = \"2h\"\n",
            backend.as_str()
        ),
    )
    .unwrap();
    let root = tmp.path().join("repo");
    let at = root.clone();
    tokio::task::spawn_blocking(move || common::init_synthetic_repo(&at, preset))
        .await
        .unwrap();
    let mut child = common::daemon_command(&state, &[("REEF_CONFIG", config.to_str().unwrap())])
        .spawn()
        .unwrap();
    let socket = state.join("daemon.sock");
    assert!(
        common::wait_for_socket(&mut child, &socket),
        "the daemon started"
    );
    let client = Client::connect(&socket).await.unwrap();
    let repo = client
        .register_repository(&root, IfNotJj::Fail)
        .await
        .unwrap()
        .repository
        .unwrap();
    let ws = client
        .create_workspace(CreateRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            ..CreateRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(ws.watcher, backend.as_str());
    // The first Watchman query has no clock yet and scans everything.
    for _ in 0..2 {
        client
            .snapshot_workspace(&repo.id, &ws.id, "")
            .await
            .unwrap();
    }
    let mut samples = Vec::new();
    for i in 0..9 {
        write(&ws, &format!("narrowed/{i}.txt"), "one change\n");
        let taken = client
            .snapshot_workspace(&repo.id, &ws.id, "")
            .await
            .unwrap();
        assert!(taken.changed);
        samples.push(taken.scan_ms);
    }
    drop(client);
    common::terminate(child);
    samples.sort_unstable();
    samples
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn excluded_dirs_do_not_trigger() {
    require_watchman!();
    let _serial = SERIAL.lock().await;
    let (daemon, repo) = fixture(Preset::Small, |_| {}).await;
    let ws = create(&daemon, &repo).await;
    subscribed(&ws, 1).await;
    let snapshots = std::sync::Arc::new(Mutex::new(Vec::new()));
    let (sink, id) = (std::sync::Arc::clone(&snapshots), id_of(&ws));
    let manager = &daemon.handle.state().manager;
    manager.scheduler().on_snapshot(Box::new(move |outcome| {
        if outcome.workspace == id {
            sink.lock().unwrap().push(outcome.paths_count);
        }
    }));
    // What R1.19 reads from `reef.toml` at create; the subscription is made
    // again with the new expression.
    manager
        .set_snapshot_spec(
            id,
            SnapshotSpec {
                excludes: vec!["target/".to_owned()],
                mounts: vec![],
            },
        )
        .await
        .unwrap();
    subscribed(&ws, 2).await;
    for i in 0..100 {
        write(&ws, &format!("target/debug/{i}.o"), "obj\n");
    }
    tokio::time::sleep(FALLBACK + Duration::from_secs(1)).await;
    let seen = snapshots.lock().unwrap().clone();
    assert!(
        seen.iter().all(|paths| *paths == 0),
        "no snapshot was triggered by events: {seen:?}"
    );
    assert!(
        !seen.is_empty(),
        "the subscription's first result or the fallback timer snapshotted it"
    );

    // A write outside the excluded directory still triggers.
    write(&ws, "src/kept.rs", "fn main() {}\n");
    snapshotted(&daemon, &repo, &ws, "src/kept.rs", Duration::from_secs(3)).await;
    let row = inspect(&daemon, &repo, &ws).await;
    assert!(!tree_has(&repo.root, &row.commit_id, "target/debug/0.o").await);
    assert!(
        snapshots.lock().unwrap().iter().any(|paths| *paths > 0),
        "the write outside target/ was a triggered snapshot"
    );
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn survives_watchman_restart() {
    require_watchman!();
    let _serial = SERIAL.lock().await;
    let (daemon, repo) = fixture(Preset::Small, |_| {}).await;
    let ws = create(&daemon, &repo).await;
    subscribed(&ws, 1).await;

    let shutdown = std::process::Command::new("watchman")
        .arg("shutdown-server")
        .output()
        .unwrap();
    assert!(
        shutdown.status.success(),
        "watchman shutdown-server: {}",
        String::from_utf8_lossy(&shutdown.stderr)
    );
    write(&ws, "during-restart.txt", "x\n");
    // The fallback snapshot's own Watchman query starts the server again
    // (jj runs `watchman get-sockname` without `WATCHMAN_SOCK` in-process),
    // which took 3.4 s on an M2 Max.
    let waited = snapshotted(
        &daemon,
        &repo,
        &ws,
        "during-restart.txt",
        FALLBACK + Duration::from_secs(10),
    )
    .await;
    eprintln!("write during the restart snapshotted after {waited:.2?}");

    wait_for(
        Duration::from_secs(15),
        "the subscription to come back",
        || async { logged("watchman subscription re-established", &ws.id) >= 1 },
    )
    .await;
    wait_for(Duration::from_secs(3), "watch-failed to clear", || async {
        inspect(&daemon, &repo, &ws).await.attention.is_empty()
    })
    .await;
    write(&ws, "after-restart.txt", "y\n");
    let waited = snapshotted(
        &daemon,
        &repo,
        &ws,
        "after-restart.txt",
        Duration::from_secs(3),
    )
    .await;
    eprintln!("write after the restart snapshotted after {waited:.2?}");
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn auto_selects_by_threshold() {
    require_watchman!();
    let _serial = SERIAL.lock().await;
    for (threshold, expected) in [(100, "watchman"), (1_000_000, "notify")] {
        let (daemon, repo) = fixture(Preset::Small, |config| {
            config.file.watcher.backend = WatcherBackend::Auto;
            config.file.watcher.watchman_threshold_files = threshold;
        })
        .await;
        let ws = create(&daemon, &repo).await;
        assert_eq!(ws.watcher, expected, "threshold {threshold}");
        assert_eq!(
            inspect(&daemon, &repo, &ws).await.watcher,
            expected,
            "threshold {threshold}"
        );
        // The count was stored at registration.
        let record = daemon
            .handle
            .state()
            .db
            .repository(repo.id.parse().unwrap())
            .unwrap();
        assert!(record.tracked_files.unwrap() >= 1_000, "{record:?}");
        daemon.stop().await;
    }
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn missing_binary_is_unavailable() {
    require_watchman!();
    let _serial = SERIAL.lock().await;
    let tmp = common::short_tempdir();
    let state = tmp.path().join("state");
    let config = tmp.path().join("config.toml");
    std::fs::write(&config, "[watcher]\nbackend = \"watchman\"\n").unwrap();
    let root = tmp.path().join("repo");
    let at = root.clone();
    tokio::task::spawn_blocking(move || common::init_synthetic_repo(&at, Preset::Small))
        .await
        .unwrap();

    let mut child = common::daemon_command(
        &state,
        &[("REEF_CONFIG", config.to_str().unwrap()), ("PATH", "")],
    )
    .spawn()
    .unwrap();
    let socket = state.join("daemon.sock");
    assert!(
        common::wait_for_socket(&mut child, &socket),
        "the daemon started"
    );
    let client = Client::connect(&socket).await.unwrap();
    let registered = client
        .register_repository(&root, IfNotJj::Fail)
        .await
        .unwrap();
    let repo = registered.repository.unwrap();
    let created = client
        .create_workspace(CreateRequest {
            repo: repo.id,
            owner: OWNER.to_owned(),
            ..CreateRequest::default()
        })
        .await;
    drop(client);
    let output = common::terminate(child);
    let error = created.expect_err("create fails without watchman");
    let ClientError::Rpc { kind, message, .. } = &error else {
        panic!("not an RPC error: {error:?}");
    };
    assert_eq!(*kind, ErrorKind::Unavailable, "{message}");
    assert!(message.contains("watchman"), "{message}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no watcher for the repository's new workspaces"),
        "registration warned:\n{stderr}"
    );
}

/// R1.29: a write made the moment `Create` returns is snapshotted with no
/// fallback timer. The subscription comes up after the write, deferred for
/// up to `watchman_attach_defer_max`; its first (fresh-instance) result
/// snapshots the workspace anyway.
#[tokio::test(flavor = "multi_thread")]
async fn write_right_after_create_is_snapshotted() {
    require_watchman!();
    let _serial = SERIAL.lock().await;
    let (daemon, repo) = fixture(Preset::Small, |config| {
        let defaults = reef_daemon::config::WatcherSection::default();
        let watcher = &mut config.file.watcher;
        watcher.fallback_interval = Duration::from_mins(10);
        watcher.watchman_attach_quiet = defaults.watchman_attach_quiet;
        watcher.watchman_attach_defer_max = defaults.watchman_attach_defer_max;
    })
    .await;
    let watcher = &daemon.config.file.watcher;
    let bound = watcher.watchman_attach_defer_max
        + watcher.min_interval
        + watcher.debounce
        + Duration::from_secs(2);
    let ws = create(&daemon, &repo).await;
    write(&ws, "right-after-create.txt", "hello\n");
    assert_eq!(ws.watcher, "watchman");
    let waited = snapshotted(&daemon, &repo, &ws, "right-after-create.txt", bound).await;
    eprintln!("a write right after create reached the operation log in {waited:.2?}");
}

/// R1.29: a just-created workspace's subscription waits until no create has
/// started for `watchman_attach_quiet`, at most `watchman_attach_defer_max`;
/// what is written meanwhile is in its first snapshot; a workspace loaded
/// at daemon start does not wait.
#[tokio::test(flavor = "multi_thread")]
async fn watchman_waits_for_a_quiet_window() {
    require_watchman!();
    let _serial = SERIAL.lock().await;
    let quiet = Duration::from_secs(1);
    let cap = Duration::from_secs(5);
    let (daemon, repo) = fixture(Preset::Small, |config| {
        let watcher = &mut config.file.watcher;
        watcher.fallback_interval = Duration::from_mins(10);
        watcher.watchman_attach_quiet = quiet;
        watcher.watchman_attach_defer_max = cap;
    })
    .await;
    let established = |ws: &Workspace| logged("watchman subscription established", &ws.id);

    // The first create builds the base, which can take longer than the
    // quiet window; a create that did has nothing left to wait for.
    create(&daemon, &repo).await;

    // A, then B 500 ms later: A waits for B's create to be 1 s old.
    let a = create(&daemon, &repo).await;
    assert_eq!(a.watch_state, "waiting");
    tokio::time::sleep(Duration::from_millis(500)).await;
    let b_started = Instant::now();
    let b = create(&daemon, &repo).await;
    write(&a, "during-the-wait.txt", "x\n");
    tokio::time::sleep_until((b_started + Duration::from_millis(900)).into()).await;
    assert_eq!(established(&a), 0, "A subscribed within 1 s of B's create");
    let inspected = inspect(&daemon, &repo, &a).await;
    assert_eq!(inspected.watch_state, "waiting");
    assert_eq!(inspected.attention, "", "waiting is not watch-failed");
    subscribed(&a, 1).await;
    let subscribed_after = b_started.elapsed();
    eprintln!("A subscribed {subscribed_after:.2?} after B's create started");
    assert!(subscribed_after >= quiet);
    assert_eq!(inspect(&daemon, &repo, &a).await.watch_state, "");
    // No fallback timer (10 minutes): the subscription's first result took
    // the snapshot. That one is a full scan, after jj's own first Watchman
    // query of the workspace, while B's subscription comes up beside it:
    // over 3 s on a loaded CI runner.
    snapshotted(
        &daemon,
        &repo,
        &a,
        "during-the-wait.txt",
        Duration::from_secs(15),
    )
    .await;
    subscribed(&b, 1).await;

    // Loaded at daemon start: not deferred. A deferred watch is marked
    // waiting before `track` returns, and the restart returns after
    // tracking every workspace, so `watch_state` right after it says which
    // (a bound on the time to subscribe would only measure Watchman).
    let c = create(&daemon, &repo).await;
    let before = established(&c);
    let daemon = daemon.restart().await;
    assert_eq!(inspect(&daemon, &repo, &c).await.watch_state, "");
    subscribed(&c, before + 1).await;

    // Creates every 500 ms never leave a quiet second; D subscribes by the
    // cap anyway.
    let d = create(&daemon, &repo).await;
    let d_created = Instant::now();
    let mut others = Vec::new();
    let mut next = d_created;
    while established(&d) == 0 {
        assert!(
            d_created.elapsed() < cap + Duration::from_secs(5),
            "D did not subscribe by the cap"
        );
        // On a fixed cadence, so a slow create does not open a quiet second.
        next += Duration::from_millis(500);
        tokio::time::sleep_until(next.into()).await;
        others.push(create(&daemon, &repo).await);
    }
    let waited = d_created.elapsed();
    eprintln!(
        "D subscribed {waited:.2?} after its create, with {} creates meanwhile",
        others.len()
    );
    assert!(waited >= Duration::from_millis(4500), "{waited:?}");
}
