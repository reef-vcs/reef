//! Crash, reclaim, and load invariants end to end (R1.23), against the
//! `reef-daemon` and `reef` binaries: agents and daemons are killed with
//! SIGKILL, and what survives is checked through the API, the database,
//! the disk, and the store. Numbered as the brief numbers them.
//!
//! Every test runs on a `small` synthetic repository with the brief's short
//! configuration: `lease.default = 3s`, `grace = 1s`, `restart_grace = 2s`,
//! `heartbeat = 1s`, `tick = 500ms`, `watcher.debounce = 200ms`,
//! `fallback_interval = 5s`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Child, Output};
use std::time::{Duration, Instant};

use common::spawned::{
    KillOnDrop, SpawnedDaemon, file_at, files_at, pid, reef, workspace_dirs, ws_run,
};
use common::{SyntheticRepo, test_settings};
use nix::sys::signal::{Signal, kill};
use reef_daemon::client::Client;
use reef_daemon::config::FileConfig;
use reef_daemon::model::{ReclaimReason, ReclaimRecord, WorkspaceId};
use reef_daemon::proto::v0::{
    CreateRequest, ListWorkspacesRequest, MoveRequest, Workspace, build_event, create_request,
    move_request,
};
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::Repo;
use reef_testing::synth::Preset;
use serde_json::Value;

const OWNER: &str = "tester@host";
const LENGTH: Duration = Duration::from_secs(3);
const GRACE: Duration = Duration::from_secs(1);
const RESTART_GRACE: Duration = Duration::from_secs(2);
const FALLBACK: Duration = Duration::from_secs(5);
/// How long a test waits for a reclaim it expects: length, grace, a tick,
/// and the final snapshot, with room for a loaded CI machine.
const RECLAIM_BOUND: Duration = Duration::from_secs(15);
/// How long a test waits for setup it does not measure: `ws run` creating
/// its workspace and starting the command, the command's first writes, a
/// create's directory appearing. A loaded 3-vCPU `macos-15` runner took
/// longer than [`RECLAIM_BOUND`] to start a `ws run` (main, 2026-10-23).
const SETUP_BOUND: Duration = Duration::from_secs(60);

fn configure(config: &mut FileConfig) {
    let lease = &mut config.lease;
    lease.default = LENGTH;
    lease.grace = GRACE;
    lease.restart_grace = RESTART_GRACE;
    lease.heartbeat = Duration::from_secs(1);
    lease.tick = Duration::from_millis(500);
    config.watcher.debounce = Duration::from_millis(200);
    config.watcher.fallback_interval = FALLBACK;
}

async fn fixture() -> (SpawnedDaemon, SyntheticRepo) {
    let daemon = SpawnedDaemon::spawn(configure).await;
    let repo = daemon.register_synth(Preset::Small).await;
    (daemon, repo)
}

async fn create_with(
    client: &Client,
    repo: &SyntheticRepo,
    start: Option<create_request::Start>,
    lease_ms: u64,
) -> Workspace {
    client
        .create_workspace(CreateRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            lease_ms,
            start,
            ..CreateRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn create(client: &Client, repo: &SyntheticRepo) -> Workspace {
    create_with(client, repo, None, 0).await
}

/// The workspace as the daemon sees it, without a snapshot first.
async fn inspect(client: &Client, id: &str) -> Workspace {
    client
        .inspect_workspace("", id, false)
        .await
        .unwrap()
        .workspace
        .unwrap()
}

/// Every workspace the daemon knows, in any state.
async fn all_workspaces(client: &Client) -> Vec<Workspace> {
    client
        .list_workspaces(ListWorkspacesRequest {
            states: ["active", "releasing", "released", "reclaimed"]
                .map(str::to_owned)
                .to_vec(),
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap()
}

fn reclaims_of(daemon: &SpawnedDaemon, id: &str) -> Vec<ReclaimRecord> {
    let id: WorkspaceId = id.parse().unwrap();
    daemon
        .db()
        .reclaims(None, 1000)
        .unwrap()
        .into_iter()
        .filter(|row| row.workspace_id == id)
        .collect()
}

fn millis(time: Option<&prost_types::Timestamp>) -> i64 {
    let time = time.unwrap();
    time.seconds * 1000 + i64::from(time.nanos) / 1_000_000
}

fn now_ms() -> i64 {
    reef_daemon::model::Timestamp::now().0
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

async fn wait_for_state(client: &Client, id: &str, state: &str) {
    wait_for(
        RECLAIM_BOUND,
        &format!("workspace {id} {state}"),
        || async { inspect(client, id).await.state == state },
    )
    .await;
}

/// Opens the registered root's jj repository at head, off the runtime.
async fn open(root: &Path) -> Repo {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(Repo::open(&root, &test_settings())).unwrap()
    })
    .await
    .unwrap()
}

/// A file a test's command writes to say where it got to, outside the
/// workspace so it is not snapshotted.
fn signal_file(daemon: &SpawnedDaemon, name: &str) -> PathBuf {
    daemon.root().join(name)
}

fn env_arg(name: &str, path: &Path) -> String {
    format!("{name}={}", path.display())
}

/// The trimmed contents of `path` once `agent`, a `ws run`, or its command
/// has written a full line to it. Fails as soon as `agent` exits without
/// writing it, or after [`SETUP_BOUND`], with `ws run`'s exit status and
/// stderr, which say whether it was slow or failed.
async fn read_line_of(path: &Path, agent: &mut Child) -> String {
    let begun = Instant::now();
    loop {
        // Before the read, so a line written just before exiting is seen.
        let exited = agent.try_wait().unwrap();
        let line = std::fs::read_to_string(path).unwrap_or_default();
        if line.ends_with('\n') {
            return line.trim().to_owned();
        }
        if exited.is_some() || begun.elapsed() >= SETUP_BOUND {
            let status = if let Some(status) = exited {
                format!("exited with {status}")
            } else {
                let _ = agent.kill();
                format!("still running after {SETUP_BOUND:?}")
            };
            agent.wait().unwrap();
            let mut stderr = String::new();
            if let Some(mut pipe) = agent.stderr.take() {
                let _ = pipe.read_to_string(&mut stderr);
            }
            panic!(
                "{}: no line; `ws run` {status}; its stderr:\n{stderr}",
                path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn agent_killed_mid_edit_leaves_snapshot_and_no_directory() {
    let (daemon, repo) = fixture().await;
    let (id_file, pid_file) = (signal_file(&daemon, "id"), signal_file(&daemon, "pid"));
    let script = r#"echo "$REEF_WORKSPACE" > "$ID_FILE"; echo $$ > "$PID_FILE"
        for i in $(seq 1 1000); do echo $i >> log.txt; sleep 0.01; done; exec sleep 60"#;
    let mut agent = ws_run(
        &daemon,
        &repo.id,
        &[
            "--env",
            &env_arg("ID_FILE", &id_file),
            "--env",
            &env_arg("PID_FILE", &pid_file),
            "--",
            "sh",
            "-c",
            script,
        ],
    );
    let mut guard = KillOnDrop(vec![agent.id()]);
    let id = read_line_of(&id_file, &mut agent).await;
    let child: u32 = read_line_of(&pid_file, &mut agent).await.parse().unwrap();
    guard.0.push(child);
    let path = PathBuf::from(inspect(&daemon.client, &id).await.path);
    let log = path.join("log.txt");
    wait_for(SETUP_BOUND, "100 lines written", || async {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .count()
            >= 100
    })
    .await;
    kill(pid(agent.id()), Signal::SIGKILL).unwrap();
    kill(pid(child), Signal::SIGKILL).unwrap();
    agent.wait().unwrap();
    // Nothing writes after the kill (the loop's last `sleep 0.01` exits by
    // itself), so this is what the final snapshot must hold.
    let written = std::fs::read(&log).unwrap();
    let killed = Instant::now();

    wait_for_state(&daemon.client, &id, "reclaimed").await;
    assert!(
        killed.elapsed() < Duration::from_secs(10),
        "{:?}",
        killed.elapsed()
    );
    assert!(!path.exists());
    let reclaims = reclaims_of(&daemon, &id);
    assert_eq!(reclaims.len(), 1, "{reclaims:?}");
    assert_eq!(reclaims[0].reason, ReclaimReason::Expired);
    assert!(reclaims[0].snapshot_taken);
    let ws = inspect(&daemon.client, &id).await;
    let saved = file_at(&repo.root, &ws.commit_id, "log.txt").await.unwrap();
    assert!(String::from_utf8_lossy(&saved).lines().count() >= 100);
    assert_eq!(
        saved, written,
        "the final snapshot is not the file as killed"
    );
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn agent_killed_with_unsnapshotted_write() {
    let (daemon, repo) = fixture().await;
    let id_file = signal_file(&daemon, "id");
    // The write and both kills happen within microseconds, well inside the
    // 200 ms debounce: `ws run` (the shell's parent) first, then the shell.
    let script =
        r#"echo "$REEF_WORKSPACE" > "$ID_FILE"; echo late > late.txt; kill -9 $PPID; kill -9 $$"#;
    let mut agent = ws_run(
        &daemon,
        &repo.id,
        &[
            "--env",
            &env_arg("ID_FILE", &id_file),
            "--",
            "sh",
            "-c",
            script,
        ],
    );
    let _guard = KillOnDrop(vec![agent.id()]);
    let status = agent.wait().unwrap();
    assert_eq!(
        std::os::unix::process::ExitStatusExt::signal(&status),
        Some(9)
    );
    let id = read_line_of(&id_file, &mut agent).await;
    let path = PathBuf::from(inspect(&daemon.client, &id).await.path);
    wait_for_state(&daemon.client, &id, "reclaimed").await;
    assert!(!path.exists());
    let reclaims = reclaims_of(&daemon, &id);
    assert_eq!(reclaims.len(), 1, "{reclaims:?}");
    assert_eq!(reclaims[0].reason, ReclaimReason::Expired);
    assert!(reclaims[0].snapshot_taken);
    let ws = inspect(&daemon.client, &id).await;
    assert_eq!(
        file_at(&repo.root, &ws.commit_id, "late.txt")
            .await
            .as_deref(),
        Some(&b"late\n"[..])
    );
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn lease_expiry_without_heartbeat() {
    let (daemon, repo) = fixture().await;
    let short = create(&daemon.client, &repo).await;
    let long = create_with(&daemon.client, &repo, None, 10_000).await;
    let expires = millis(short.lease.as_ref().unwrap().expires_at.as_ref());
    wait_for_state(&daemon.client, &short.id, "reclaimed").await;
    let reclaims = reclaims_of(&daemon, &short.id);
    assert_eq!(reclaims.len(), 1, "{reclaims:?}");
    assert_eq!(reclaims[0].reason, ReclaimReason::Expired);
    let grace = i64::try_from(GRACE.as_millis()).unwrap();
    assert!(
        reclaims[0].at.0 >= expires + grace,
        "reclaimed at {} before expiry {expires} and grace",
        reclaims[0].at.0
    );
    assert!(!Path::new(&short.path).exists());
    let long = inspect(&daemon.client, &long.id).await;
    assert_eq!(long.state, "active");
    assert!(Path::new(&long.path).is_dir());
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_in_grace_revives() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon.client, &repo).await;
    let lease = ws.lease.clone().unwrap();
    let expires = millis(lease.expires_at.as_ref());
    // Just past the expiry, leaving most of the 1 s grace for the RPC.
    let wait = u64::try_from(expires + 50 - now_ms()).unwrap_or(0);
    tokio::time::sleep(Duration::from_millis(wait)).await;
    let beat = daemon
        .client
        .heartbeat_workspace("", &ws.id, &lease.id)
        .await
        .unwrap();
    assert!(millis(beat.expires_at.as_ref()) > expires);
    let revived = inspect(&daemon.client, &ws.id).await;
    assert_eq!(revived.state, "active");
    assert_eq!(reclaims_of(&daemon, &ws.id), []);

    // Then left to lapse: past the grace a heartbeat is refused.
    wait_for_state(&daemon.client, &ws.id, "reclaimed").await;
    let error = daemon
        .client
        .heartbeat_workspace("", &ws.id, &lease.id)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict, "{error:?}");
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn two_workspaces_on_one_change() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let first = create(client, &repo).await;
    // An empty change is abandoned when its workspace is forgotten.
    std::fs::write(Path::new(&first.path).join("c.txt"), "c").unwrap();
    client
        .release_workspace("", &first.id, true, false)
        .await
        .unwrap();
    let change = first.change_id.clone();
    // `change_id` is hex; `--change` takes jj's reverse hex, as the CLI
    // prints it.
    let rev = reef_daemon::client::json::reverse_hex(&change);
    let on_change = || Some(create_request::Start::Change(rev.clone()));
    let a = create_with(client, &repo, on_change(), 60_000).await;
    let b = create_with(client, &repo, on_change(), 60_000).await;

    let write_and_snapshot = |ws: &Workspace, name: &str| {
        std::fs::write(Path::new(&ws.path).join(name), name).unwrap();
        let (client, id) = (client.clone(), ws.id.clone());
        async move { client.snapshot_workspace("", &id, "").await.unwrap() }
    };
    write_and_snapshot(&a, "a.txt").await;
    write_and_snapshot(&b, "b.txt").await;
    let (a_now, b_now) = (inspect(client, &a.id).await, inspect(client, &b.id).await);
    for ws in [&a_now, &b_now] {
        assert_eq!(ws.state, "active");
        assert_eq!(ws.change_id, change);
    }
    let has = |ws: &Workspace, path: &'static str| {
        let (root, commit) = (repo.root.clone(), ws.commit_id.clone());
        async move { file_at(&root, &commit, path).await.is_some() }
    };
    assert!(has(&a_now, "a.txt").await && !has(&a_now, "b.txt").await);
    assert!(has(&b_now, "b.txt").await && !has(&b_now, "a.txt").await);
    let heads = open(&repo.root).await.heads().unwrap();
    let on_c = heads
        .iter()
        .filter(|head| head.change_id.as_str() == change)
        .count();
    assert_eq!(on_c, 2, "{heads:?}");
    assert!(
        [&a_now, &b_now]
            .iter()
            .any(|ws| ws.divergent && ws.attention == "divergent"),
        "{a_now:?}\n{b_now:?}"
    );

    // Both keep working, each on its own files.
    write_and_snapshot(&a, "a2.txt").await;
    write_and_snapshot(&b, "b2.txt").await;
    let (a_now, b_now) = (inspect(client, &a.id).await, inspect(client, &b.id).await);
    for (ws, mine, theirs) in [
        (&a_now, ["a.txt", "a2.txt"], ["b.txt", "b2.txt"]),
        (&b_now, ["b.txt", "b2.txt"], ["a.txt", "a2.txt"]),
    ] {
        for path in mine {
            assert!(has(ws, path).await, "{} lacks {path}", ws.id);
        }
        for path in theirs {
            assert!(!has(ws, path).await, "{} has {path}", ws.id);
        }
    }
    client
        .release_workspace("", &a.id, true, false)
        .await
        .unwrap();
    client
        .release_workspace("", &b.id, true, false)
        .await
        .unwrap();
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn daemon_killed_mid_snapshot_recovers() {
    let (mut daemon, repo) = fixture().await;
    let ws = create_with(&daemon.client, &repo, None, 0).await;
    let lease = ws.lease.clone().unwrap().id;
    let root = PathBuf::from(&ws.path);
    let files: Vec<String> = tracked_files(&root).into_iter().take(500).collect();
    assert_eq!(files.len(), 500);

    for (round, delay_ms) in [5_u64, 20, 50, 100].into_iter().enumerate() {
        for file in &files {
            std::fs::write(root.join(file), format!("round {round}: {file}\n")).unwrap();
        }
        let client = daemon.client.clone();
        let id = ws.id.clone();
        let snapshot = tokio::spawn(async move { client.snapshot_workspace("", &id, "").await });
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
        daemon.kill();
        // Its outcome depends on where the kill landed; either is fine.
        let _ = snapshot.await.unwrap();
        let log_mark = log_events(&daemon).len();
        let restarted = now_ms();
        daemon = daemon.respawn().await;
        let client = &daemon.client;

        client.status().await.unwrap();
        let after = inspect(client, &ws.id).await;
        assert_eq!(after.state, "active", "round {round}");
        let restart_grace = i64::try_from(RESTART_GRACE.as_millis()).unwrap();
        let expires = millis(after.lease.as_ref().unwrap().expires_at.as_ref());
        assert!(
            expires >= restarted + restart_grace,
            "round {round}: expires {expires} before restart {restarted} + grace"
        );
        client
            .heartbeat_workspace("", &ws.id, &lease)
            .await
            .unwrap();
        // Nothing reclaimed on the first ticks.
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert_eq!(reclaims_of(&daemon, &ws.id), [], "round {round}");
        client
            .heartbeat_workspace("", &ws.id, &lease)
            .await
            .unwrap();

        let snapshot = client.snapshot_workspace("", &ws.id, "").await.unwrap();
        let saved = files_at(&repo.root, &snapshot.commit_id, &files).await;
        for (file, saved) in files.iter().zip(saved) {
            assert_eq!(
                saved.as_deref(),
                Some(std::fs::read(root.join(file)).unwrap().as_slice()),
                "round {round}: {file}"
            );
        }
        let errors: Vec<Value> = log_events(&daemon)
            .into_iter()
            .skip(log_mark)
            .filter(|event| event["level"] == "ERROR")
            .collect();
        let stale = errors
            .iter()
            .filter(|event| event["message"] == "stale working copy recovered")
            .count();
        assert!(
            errors.len() == stale && stale <= 1,
            "round {round}: errors after the respawn: {errors:#?}"
        );
    }
}

/// Every event in the daemon's log, oldest first.
fn log_events(daemon: &SpawnedDaemon) -> Vec<Value> {
    daemon.log_lines("")
}

/// The regular files under `root`, relative, sorted.
fn tracked_files(root: &Path) -> Vec<String> {
    let mut files: Vec<String> = walkdir::WalkDir::new(root)
        .into_iter()
        .map(Result::unwrap)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            entry
                .path()
                .strip_prefix(root)
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    files.sort();
    files
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn daemon_killed_during_create() {
    let mut daemon = SpawnedDaemon::spawn(configure).await;
    let repo = daemon.register_synth(Preset::Medium).await;
    let ws_root = daemon.state_dir.join("repos").join(&repo.id).join("ws");
    // Builds the base, so the next creates' window between the directory
    // and the row is what is left: materialize, jj, seed, insert.
    let warm = create_with(&daemon.client, &repo, None, 60_000).await;
    let mut killed_mid_create = false;
    for attempt in 0..10 {
        let known: BTreeSet<String> = workspace_dirs(&ws_root);
        let client = daemon.client.clone();
        let id = repo.id.clone();
        let create = tokio::spawn(async move {
            client
                .create_workspace(CreateRequest {
                    repo: id,
                    owner: OWNER.to_owned(),
                    lease_ms: 60_000,
                    ..CreateRequest::default()
                })
                .await
        });
        // Polled on a thread of its own, so the kill lands within a
        // millisecond of the directory appearing.
        let watch = ws_root.clone();
        let appeared = tokio::task::spawn_blocking(move || {
            let begun = Instant::now();
            while begun.elapsed() < SETUP_BOUND {
                if workspace_dirs(&watch).difference(&known).next().is_some() {
                    return true;
                }
                std::thread::sleep(Duration::from_micros(500));
            }
            false
        })
        .await
        .unwrap();
        assert!(appeared, "attempt {attempt}: no directory appeared");
        daemon.kill();
        let outcome = create.await.unwrap();
        daemon = daemon.respawn().await;
        check_rows_match_directories(&daemon, &repo, &ws_root).await;
        if outcome.is_err() {
            killed_mid_create = true;
            break;
        }
    }
    assert!(killed_mid_create, "every create returned before the kill");
    // The daemon still creates, and the warm workspace is untouched.
    let ws = create(&daemon.client, &repo).await;
    assert!(Path::new(&ws.path).is_dir());
    assert_eq!(inspect(&daemon.client, &warm.id).await.state, "active");
}

/// Every workspace directory has an active row and every active row a
/// directory; each active workspace takes a snapshot.
async fn check_rows_match_directories(
    daemon: &SpawnedDaemon,
    repo: &SyntheticRepo,
    ws_root: &Path,
) {
    let rows = all_workspaces(&daemon.client).await;
    let active: BTreeSet<String> = rows
        .iter()
        .filter(|ws| ws.state == "active")
        .map(|ws| ws.id.clone())
        .collect();
    let dirs = workspace_dirs(ws_root);
    assert_eq!(
        dirs, active,
        "directories without an active row, or active rows without a directory"
    );
    for id in &active {
        daemon
            .client
            .snapshot_workspace(&repo.id, id, "")
            .await
            .unwrap_or_else(|error| panic!("{id}: {error:?}"));
    }
}

/// `reef.toml` declaring store `deps` (R1.20's fixture): its build copies
/// the lockfile in, and it is mounted at `vendor/deps`.
const STORE_SPEC: &str = r#"
[[store]]
name = "deps"
lockfile = "deps.lock"
mount = "vendor/deps"
build = ["sh", "-c", "mkdir -p \"$REEF_STORE_DIR\" && cp deps.lock \"$REEF_STORE_DIR/manifest\""]
"#;

/// Commits the store spec at the root and builds the store.
async fn with_store(daemon: &SpawnedDaemon, repo: &SyntheticRepo) {
    std::fs::write(repo.root.join("reef.toml"), STORE_SPEC).unwrap();
    std::fs::write(repo.root.join("deps.lock"), "v1\n").unwrap();
    let root = repo.root.clone();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let mut repo = Repo::open(&root, &test_settings()).await.unwrap();
            repo.snapshot("add reef.toml").await.unwrap();
        });
    })
    .await
    .unwrap();
    let mut stream = daemon
        .client
        .build_store(&repo.id, "deps", "", Duration::from_secs(60), false)
        .await
        .unwrap();
    let mut done = None;
    while let Some(event) = stream.message().await.unwrap() {
        if let Some(build_event::Event::Done(result)) = event.event {
            done = Some(result);
        }
    }
    assert!(done.is_some(), "the store build did not finish");
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn restart_reestablishes_mounts_and_watchers() {
    let (daemon, repo) = fixture().await;
    with_store(&daemon, &repo).await;
    let ws = create_with(&daemon.client, &repo, None, 60_000).await;
    let root = PathBuf::from(&ws.path);
    let manifest = root.join("vendor/deps/manifest");
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), "v1\n");

    let daemon = daemon.respawn().await;
    let after = inspect(&daemon.client, &ws.id).await;
    assert_eq!(after.state, "active");
    assert_eq!(after.attention, "", "{after:?}");
    assert_eq!(after.stores.len(), 1, "{after:?}");
    assert_eq!(std::fs::read_to_string(&manifest).unwrap(), "v1\n");

    // The watcher (or at worst the fallback timer) is back.
    let before = after.commit_id.clone();
    std::fs::write(root.join("after-restart.txt"), "x").unwrap();
    wait_for(
        FALLBACK + Duration::from_secs(5),
        "snapshot after restart",
        || async { inspect(&daemon.client, &ws.id).await.commit_id != before },
    )
    .await;
    let now = inspect(&daemon.client, &ws.id).await;
    assert!(
        file_at(&repo.root, &now.commit_id, "after-restart.txt")
            .await
            .is_some()
    );
}

// 8, beyond the brief: no watcher saw a write made while the daemon was
// down, so a loaded workspace waits only `fallback_interval` for its first
// fallback snapshot, not `watched_fallback_interval` (10 minutes here).
#[tokio::test(flavor = "multi_thread")]
async fn write_while_daemon_down_is_snapshotted() {
    let (mut daemon, repo) = fixture().await;
    let ws = create_with(&daemon.client, &repo, None, 60_000).await;
    let root = PathBuf::from(&ws.path);
    let before = inspect(&daemon.client, &ws.id).await.commit_id;

    daemon.kill();
    std::fs::write(root.join("while-down.txt"), "x").unwrap();
    let daemon = daemon.respawn().await;
    wait_for(
        FALLBACK + Duration::from_secs(5),
        "snapshot of the write made while the daemon was down",
        || async { inspect(&daemon.client, &ws.id).await.commit_id != before },
    )
    .await;
    let now = inspect(&daemon.client, &ws.id).await;
    assert!(
        file_at(&repo.root, &now.commit_id, "while-down.txt")
            .await
            .is_some()
    );
}

// 9
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn one_inotify_instance() {
    let (daemon, repo) = fixture().await;
    for _ in 0..10 {
        create_with(&daemon.client, &repo, None, 60_000).await;
    }
    let instances = std::fs::read_dir(format!("/proc/{}/fd", daemon.pid))
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| target.to_string_lossy() == "anon_inode:inotify")
        .count();
    assert_eq!(instances, 1);
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn store_shared_by_twenty() {
    let (daemon, repo) = fixture().await;
    with_store(&daemon, &repo).await;
    let mut ids = Vec::new();
    for _ in 0..20 {
        ids.push(create_with(&daemon.client, &repo, None, 60_000).await.id);
    }
    let in_use = || async {
        let stores = daemon.client.list_stores(&repo.id).await.unwrap();
        assert_eq!(stores.len(), 1, "{stores:?}");
        stores[0].in_use_by
    };
    assert_eq!(in_use().await, 20);
    for id in &ids {
        daemon
            .client
            .release_workspace("", id, true, false)
            .await
            .unwrap();
    }
    assert_eq!(in_use().await, 0);
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn move_keeps_lease_and_directory() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let other = create(client, &repo).await;
    std::fs::write(Path::new(&other.path).join("other.txt"), "other").unwrap();
    client
        .release_workspace("", &other.id, true, false)
        .await
        .unwrap();
    let ws = create_with(client, &repo, None, 60_000).await;
    let moved = client
        .move_workspace(MoveRequest {
            workspace: ws.id.clone(),
            target: Some(move_request::Target::Change(
                reef_daemon::client::json::reverse_hex(&other.change_id),
            )),
            ..MoveRequest::default()
        })
        .await
        .unwrap();
    assert_ne!(moved.op_id, "");
    let moved = moved.workspace.unwrap();
    assert_eq!(moved.change_id, other.change_id);
    assert_eq!(moved.lease.unwrap().id, ws.lease.unwrap().id);
    assert_eq!(moved.path, ws.path);
    assert_eq!(moved.state, "active");
    assert_eq!(
        std::fs::read_to_string(Path::new(&ws.path).join("other.txt")).unwrap(),
        "other"
    );
    assert_eq!(reclaims_of(&daemon, &ws.id), []);
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn same_cli_both_platforms() {
    let daemon = SpawnedDaemon::spawn(configure).await;
    let root = daemon.root().join("cli");
    let at = root.clone();
    tokio::task::spawn_blocking(move || common::init_synthetic_repo(&at, Preset::Small))
        .await
        .unwrap();
    let run = |args: &[&str]| -> Value {
        let output = reef(&daemon)
            .args(["-o", "json"])
            .args(args)
            .output()
            .unwrap();
        assert_success(args, &output);
        serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("{args:?}: {error}: {}", text(&output.stdout)))
    };
    let root_arg = root.to_str().unwrap();
    let mut shapes = serde_json::Map::new();
    let registered = run(&["repo", "register", root_arg]);
    let repo = registered["repository"]["id"]
        .as_str()
        .or_else(|| registered["id"].as_str())
        .unwrap_or_else(|| panic!("no repository id in {registered}"))
        .to_owned();
    shapes.insert("repo register".into(), shape(&registered));
    let created = run(&["ws", "create", "--repo", &repo, "--lease", "1m"]);
    let id = created["id"].as_str().unwrap().to_owned();
    shapes.insert("ws create".into(), shape(&created));
    shapes.insert(
        "ws run".into(),
        shape(&run(&[
            "ws",
            "run",
            "--repo",
            &repo,
            "--",
            "sh",
            "-c",
            "echo hi > hi.txt",
        ])),
    );
    shapes.insert("ws list".into(), shape(&run(&["ws", "list", "--all"])));
    shapes.insert("ws inspect".into(), shape(&run(&["ws", "inspect", &id])));
    shapes.insert("ws release".into(), shape(&run(&["ws", "release", &id])));
    shapes.insert("ws gc".into(), shape(&run(&["ws", "gc", "--repo", &repo])));

    let target = Path::new(env!("CARGO_BIN_EXE_reef-daemon"))
        .parent()
        .and_then(Path::parent)
        .unwrap();
    let out = target.join(format!("cli-shape-{}.json", std::env::consts::OS));
    std::fs::write(
        &out,
        serde_json::to_string_pretty(&Value::Object(shapes)).unwrap() + "\n",
    )
    .unwrap();
}

fn assert_success(args: &[&str], output: &Output) {
    assert!(
        output.status.success(),
        "reef {args:?}: {}\n{}",
        output.status,
        text(&output.stderr)
    );
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `value`'s key paths, sorted: `a.b` for an object member, `a[]` for an
/// array's elements (merged across them). Values are dropped.
fn shape(value: &Value) -> Value {
    fn walk(value: &Value, path: &str, out: &mut BTreeSet<String>) {
        match value {
            Value::Object(members) => {
                for (key, member) in members {
                    let path = if path.is_empty() {
                        key.clone()
                    } else {
                        format!("{path}.{key}")
                    };
                    out.insert(path.clone());
                    walk(member, &path, out);
                }
            }
            Value::Array(items) => {
                for item in items {
                    walk(item, &format!("{path}[]"), out);
                }
            }
            _ => {}
        }
    }
    let mut keys = BTreeSet::new();
    walk(value, "", &mut keys);
    Value::Array(keys.into_iter().map(Value::String).collect())
}

/// Not a test of the daemon: the configuration the tests write is one the
/// binary reads back unchanged.
#[test]
fn file_config_round_trips() {
    let mut config = FileConfig::default();
    configure(&mut config);
    let text = toml::to_string(&config).unwrap();
    assert_eq!(toml::from_str::<FileConfig>(&text).unwrap(), config);
}
