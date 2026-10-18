//! Leases end to end (R1.15): heartbeat, expiry and reclaim, grace, the
//! restart grace, and the reclaim pipeline's refusals. Numbered as the
//! brief numbers them; the last one is the shutdown snapshot.
//!
//! The brief's test configuration: `lease.default = 2s`, `max = 10s`,
//! `grace = 1s`, `restart_grace = 3s`, `tick = 500ms`, on the `small`
//! preset.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::time::{Duration, Instant};

use common::{SyntheticRepo, TestDaemon, test_settings};
use jj_lib::backend::CommitId;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use reef_daemon::backend::{BackendChoice, BackendKind};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::daemon::Daemon;
use reef_daemon::error::detail_of;
use reef_daemon::model::{LeaseEnd, ReclaimReason, ReclaimRecord, WorkspaceId, WorkspaceState};
use reef_daemon::proto::v0::{CreateRequest, Workspace, lease};
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::oplog::OpEntry;
use reef_lib::jj::{CommitIdHex, Repo};
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";
const LENGTH: Duration = Duration::from_secs(2);
const GRACE: Duration = Duration::from_secs(1);
const RESTART_GRACE: Duration = Duration::from_secs(3);
const TICK: Duration = Duration::from_millis(500);
/// How long a test waits for a reclaim it expects: length, grace, and a
/// tick, with room for a slow CI machine.
const RECLAIM_BOUND: Duration = Duration::from_secs(15);

fn configure(config: &mut reef_daemon::config::Config) {
    let lease = &mut config.file.lease;
    lease.default = LENGTH;
    lease.max = Duration::from_secs(10);
    lease.grace = GRACE;
    lease.restart_grace = RESTART_GRACE;
    lease.heartbeat = Duration::from_millis(500);
    lease.tick = TICK;
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(configure).await
}

/// [`fixture`] with workspace roots that are plain directories: on Linux
/// the copy backend, not an overlay (whose root is a mountpoint, which
/// cannot be removed or locked with `chmod` from under the mount), when
/// `fuse-overlayfs` is installed or the tests run as root.
async fn fixture_on_plain_directories() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|config| {
        configure(config);
        if cfg!(target_os = "linux") {
            config.file.workspace.backend = BackendChoice::Named(BackendKind::Copy);
        }
    })
    .await
}

async fn fixture_with(
    configure: impl FnOnce(&mut reef_daemon::config::Config),
) -> (TestDaemon, SyntheticRepo) {
    // `TEST_LOG=reef_daemon=debug` shows the daemons' logs.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("TEST_LOG"))
        .with_test_writer()
        .try_init();
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, configure).await;
    // Only final and explicit snapshots, so the op log is the reclaim's.
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn create_with(daemon: &TestDaemon, repo: &SyntheticRepo, lease_ms: u64) -> Workspace {
    daemon
        .client
        .create_workspace(CreateRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            lease_ms,
            ..CreateRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn create(daemon: &TestDaemon, repo: &SyntheticRepo) -> Workspace {
    create_with(daemon, repo, 0).await
}

fn id_of(ws: &Workspace) -> WorkspaceId {
    ws.id.parse().unwrap()
}

fn lease_id(ws: &Workspace) -> String {
    ws.lease.as_ref().unwrap().id.clone()
}

async fn inspect(client: &Client, ws: &Workspace) -> Workspace {
    client
        .inspect_workspace("", &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap()
}

fn millis(time: Option<&prost_types::Timestamp>) -> i64 {
    let time = time.unwrap();
    time.seconds * 1000 + i64::from(time.nanos) / 1_000_000
}

fn now_ms() -> i64 {
    reef_daemon::model::Timestamp::now().0
}

/// The kind an RPC failed with, checking the status code and that the
/// status carries an `ErrorDetail` that agrees.
fn kind_of(error: &ClientError) -> ErrorKind {
    let ClientError::Rpc { kind, status, .. } = error else {
        panic!("not an RPC error: {error:?}");
    };
    let detail = detail_of(status).expect("an ErrorDetail in the status");
    assert_eq!(detail.kind, reef_daemon::error::kind_name(*kind));
    if *kind == ErrorKind::Conflict {
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }
    assert_eq!(error.kind(), *kind);
    *kind
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

async fn wait_for_state(client: &Client, ws: &Workspace, state: &str) {
    wait_for(RECLAIM_BOUND, &format!("workspace {state}"), || async {
        inspect(client, ws).await.state == state
    })
    .await;
}

fn reclaims_of(daemon: &TestDaemon, ws: &Workspace) -> Vec<ReclaimRecord> {
    let id = id_of(ws);
    daemon
        .handle
        .state()
        .db
        .reclaims(None, 100)
        .unwrap()
        .into_iter()
        .filter(|row| row.workspace_id == id)
        .collect()
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

/// The operation log, newest first.
async fn op_log(root: &Path) -> Vec<OpEntry> {
    let repo = open(root).await;
    tokio::task::spawn_blocking(move || pollster::block_on(repo.op_log(500)).unwrap())
        .await
        .unwrap()
}

/// Whether `path` is in `commit`'s tree.
async fn tree_has(root: &Path, commit: &str, path: &str) -> bool {
    let repo = open(root).await;
    let (commit, path) = (commit.to_owned(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let id = CommitId::try_from_hex(&commit).unwrap();
        let commit = repo.head().store().get_commit(&id).unwrap();
        let path = RepoPathBuf::from_internal_string(path).unwrap();
        !pollster::block_on(commit.tree().path_value(&path))
            .unwrap()
            .is_absent()
    })
    .await
    .unwrap()
}

/// Whether `commit` is a visible head of the repository.
async fn is_visible_head(root: &Path, commit: &str) -> bool {
    let repo = open(root).await;
    let id = CommitId::try_from_hex(commit).unwrap();
    repo.head().view().heads().contains(&id)
}

fn state_dir_of(daemon: &TestDaemon, repo: &SyntheticRepo, ws: &Workspace) -> std::path::PathBuf {
    daemon
        .state_dir()
        .join("repos")
        .join(&repo.id)
        .join("wc")
        .join(&ws.id)
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_extends() {
    let (daemon, repo) = fixture().await;
    // The maximum, not the 2 s default: with every test starting at once a
    // loaded runner can take longer than the default's 3 s to send the
    // heartbeat, and expiry is not what this test checks.
    let ws = create_with(&daemon, &repo, 10_000).await;
    let issued = ws.lease.clone().unwrap();
    assert_eq!(issued.state(), lease::State::Active);
    assert_eq!(
        issued.heartbeat_every,
        Some(prost_types::Duration {
            seconds: 0,
            nanos: 500_000_000
        }),
        "lease.heartbeat, under a third of the length"
    );
    let short = create(&daemon, &repo).await.lease.unwrap();
    assert_eq!(
        short.heartbeat_every, issued.heartbeat_every,
        "a third of the 2 s default is still above 500 ms"
    );

    tokio::time::sleep(Duration::from_secs(1)).await;
    let renewed = daemon
        .client
        .heartbeat_workspace("", &ws.id[..8], &issued.id)
        .await
        .unwrap();
    assert_eq!(renewed.lease_id, issued.id);
    assert_eq!(renewed.heartbeat_every, issued.heartbeat_every);

    let lease = inspect(&daemon.client, &ws).await.lease.unwrap();
    assert_eq!(
        millis(lease.expires_at.as_ref()),
        millis(renewed.expires_at.as_ref())
    );
    let heartbeat_at = millis(lease.last_heartbeat_at.as_ref());
    assert!(heartbeat_at >= millis(issued.issued_at.as_ref()) + 900);
    assert_eq!(
        millis(lease.expires_at.as_ref()) - heartbeat_at,
        10_000,
        "now plus the length"
    );
    // By the time between issue and heartbeat: about 1 s, more on a busy
    // machine.
    let moved = millis(lease.expires_at.as_ref()) - millis(issued.expires_at.as_ref());
    assert_eq!(moved, heartbeat_at - millis(issued.issued_at.as_ref()));
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn expired_lease_is_reclaimed() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let state_dir = state_dir_of(&daemon, &repo, &ws);
    std::fs::write(Path::new(&ws.path).join("last-words.txt"), "unsaved\n").unwrap();

    wait_for_state(&daemon.client, &ws, "reclaimed").await;
    assert!(!Path::new(&ws.path).exists(), "directory gone");
    assert!(!state_dir.exists(), "wc/<id> gone");
    let inspected = inspect(&daemon.client, &ws).await;
    let lease = inspected.lease.unwrap();
    assert_eq!(lease.state(), lease::State::Ended);
    assert_eq!(lease.end_reason, "expired");

    let reclaims = reclaims_of(&daemon, &ws);
    assert_eq!(reclaims.len(), 1);
    assert_eq!(reclaims[0].reason, ReclaimReason::Expired);
    assert!(reclaims[0].snapshot_taken);
    let final_op = reclaims[0].final_op_id.clone().expect("the file was new");

    let log = op_log(&repo.root).await;
    assert!(
        log[0]
            .description
            .starts_with(&format!("reef: release workspace {}", ws.id)),
        "{:?}",
        log[0].description
    );
    assert_eq!(log[0].parents.len(), 1);
    assert_eq!(log[0].parents[0].to_string(), final_op);
    assert_eq!(
        log[1].description,
        format!(
            "reef: final snapshot of workspace {} (lease expired)",
            ws.id
        )
    );
    let commit = daemon
        .handle
        .state()
        .db
        .workspace(id_of(&ws))
        .unwrap()
        .commit_id;
    assert!(tree_has(&repo.root, &commit, "last-words.txt").await);
    let commit: CommitIdHex = commit.parse().unwrap();
    assert!(
        open(&repo.root)
            .await
            .workspaces_on_commit(&commit)
            .is_empty(),
        "no reef-<id> workspace is left in the view"
    );
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn grace_revives() {
    let (daemon, repo) = fixture().await;
    let begun = Instant::now();
    let ws = create(&daemon, &repo).await;
    let expires_at = millis(ws.lease.as_ref().unwrap().expires_at.as_ref());
    // Early in the grace, so a slow machine still heartbeats inside it. A
    // create slower than the lease (a loaded machine) is in the grace
    // already: no wait.
    let wait = u64::try_from(expires_at + 200 - now_ms()).unwrap_or(0);
    tokio::time::sleep(Duration::from_millis(wait)).await;
    assert_eq!(
        inspect(&daemon.client, &ws).await.lease.unwrap().state(),
        lease::State::InGrace
    );
    daemon
        .client
        .heartbeat_workspace("", &ws.id, &lease_id(&ws))
        .await
        .unwrap();

    tokio::time::sleep(Duration::from_secs(4).saturating_sub(begun.elapsed())).await;
    let inspected = inspect(&daemon.client, &ws).await;
    assert_eq!(inspected.state, "active");
    assert_eq!(inspected.lease.unwrap().state(), lease::State::Active);
    assert!(Path::new(&ws.path).is_dir());
    assert_eq!(reclaims_of(&daemon, &ws), Vec::<ReclaimRecord>::new());
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn heartbeat_after_reclaim_is_conflict() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    wait_for_state(&daemon.client, &ws, "reclaimed").await;
    let error = daemon
        .client
        .heartbeat_workspace(&repo.id, &ws.id, &lease_id(&ws))
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::Conflict);
    assert!(error.to_string().contains("reclaimed at"), "{error}");
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn superseded_lease_is_conflict() {
    let (daemon, repo) = fixture().await;
    let ws = create_with(&daemon, &repo, 10_000).await;
    let random = reef_daemon::model::LeaseId::generate().as_hex();
    let error = daemon
        .client
        .heartbeat_workspace(&repo.id, &ws.id, &random)
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::Conflict);
    assert!(error.to_string().contains("lease superseded"), "{error}");

    let malformed = daemon
        .client
        .heartbeat_workspace(&repo.id, &ws.id, "not-a-lease")
        .await
        .unwrap_err();
    assert_eq!(kind_of(&malformed), ErrorKind::InvalidArgument);
    let unknown = daemon
        .client
        .heartbeat_workspace(&repo.id, "ffffffffffffffff", &lease_id(&ws))
        .await
        .unwrap_err();
    assert_eq!(kind_of(&unknown), ErrorKind::NotFound);
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn too_long_is_invalid() {
    let (daemon, repo) = fixture().await;
    let error = daemon
        .client
        .create_workspace(CreateRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            lease_ms: 11_000,
            ..CreateRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(kind_of(&error), ErrorKind::InvalidArgument);
    let ws = create_with(&daemon, &repo, 10_000).await;
    assert_eq!(
        ws.lease.unwrap().length_ms,
        10_000,
        "the maximum is allowed"
    );
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn restart_applies_grace() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo).await;
    let TestDaemon {
        handle,
        client,
        tmp,
        config,
    } = daemon;
    drop(client);
    handle.shutdown();
    handle.join().await.unwrap();
    // Past expiry and grace while the daemon is down.
    tokio::time::sleep(LENGTH + GRACE + Duration::from_millis(500)).await;

    let started = now_ms();
    let handle = Daemon::start(config.clone()).await.unwrap();
    let client = Client::connect(handle.socket_path()).await.unwrap();
    let daemon = TestDaemon {
        handle,
        client,
        tmp,
        config,
    };
    daemon.handle.state().manager.scheduler().pause_watcher();
    let reclaimed = daemon
        .handle
        .state()
        .manager
        .leases()
        .tick_once()
        .await
        .unwrap();
    assert!(reclaimed.is_empty(), "the first tick reclaims nothing");
    let inspected = inspect(&daemon.client, &ws).await;
    assert_eq!(inspected.state, "active");
    let expires_at = millis(inspected.lease.unwrap().expires_at.as_ref());
    assert!(
        expires_at >= started + 3_000,
        "restart grace: {expires_at} < {started} + 3 s"
    );

    let elapsed = wait_for(RECLAIM_BOUND, "reclaimed after the grace", || async {
        inspect(&daemon.client, &ws).await.state == "reclaimed"
    })
    .await;
    assert!(
        elapsed >= RESTART_GRACE.saturating_sub(Duration::from_millis(500)),
        "reclaimed after {elapsed:?}, inside the restart grace"
    );
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn final_snapshot_failure_keeps_directory() {
    if nix::unistd::geteuid().is_root() {
        eprintln!("skipped: root ignores directory permissions");
        return;
    }
    let (daemon, repo) = fixture_on_plain_directories().await;
    let ws = create(&daemon, &repo).await;
    let root = Path::new(&ws.path);
    std::fs::write(root.join("last-words.txt"), "unsaved\n").unwrap();
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o000)).unwrap();

    wait_for(RECLAIM_BOUND, "final-snapshot-failed", || async {
        inspect(&daemon.client, &ws).await.attention == "final-snapshot-failed"
    })
    .await;
    // Two more ticks, each retrying and failing. Each attempt holds the
    // workspace `releasing` until its final snapshot fails, so read it
    // between attempts.
    tokio::time::sleep(TICK * 2 + Duration::from_millis(100)).await;
    let mut inspected = inspect(&daemon.client, &ws).await;
    let begun = Instant::now();
    while inspected.state == "releasing" && begun.elapsed() < RECLAIM_BOUND {
        tokio::time::sleep(Duration::from_millis(20)).await;
        inspected = inspect(&daemon.client, &ws).await;
    }
    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(inspected.state, "active");
    assert_eq!(inspected.attention, "final-snapshot-failed");
    assert!(inspected.lease.unwrap().end_reason.is_empty(), "lease open");
    assert!(root.is_dir(), "never torn down");
    assert!(root.join("last-words.txt").is_file());
    assert_eq!(reclaims_of(&daemon, &ws), Vec::<ReclaimRecord>::new());

    wait_for_state(&daemon.client, &ws, "reclaimed").await;
    let reclaims = reclaims_of(&daemon, &ws);
    assert_eq!(reclaims.len(), 1);
    assert!(reclaims[0].snapshot_taken);
    assert!(reclaims[0].final_op_id.is_some(), "the file was saved");
    assert_eq!(
        inspect(&daemon.client, &ws).await.attention,
        "",
        "cleared once reclaimed"
    );
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn missing_root_is_reclaimed_without_snapshot() {
    let (daemon, repo) = fixture_on_plain_directories().await;
    let ws = create(&daemon, &repo).await;
    std::fs::remove_dir_all(&ws.path).unwrap();

    wait_for_state(&daemon.client, &ws, "reclaimed").await;
    let reclaims = reclaims_of(&daemon, &ws);
    assert_eq!(reclaims.len(), 1);
    assert_eq!(reclaims[0].reason, ReclaimReason::Expired);
    assert!(!reclaims[0].snapshot_taken);
    assert_eq!(reclaims[0].final_op_id, None);
    let detail = reclaims[0].detail.as_deref().unwrap_or_default();
    assert!(detail.contains("workspace root missing"), "{detail}");
    let final_snapshot = format!("reef: final snapshot of workspace {}", ws.id);
    assert!(
        !op_log(&repo.root)
            .await
            .iter()
            .any(|op| op.description.starts_with(&final_snapshot)),
        "no final snapshot operation"
    );
    assert!(!state_dir_of(&daemon, &repo, &ws).exists());
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn release_uses_the_pipeline() {
    let (daemon, repo) = fixture().await;
    let ws = create_with(&daemon, &repo, 10_000).await;
    daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    let reclaims = reclaims_of(&daemon, &ws);
    assert_eq!(reclaims.len(), 1);
    assert_eq!(reclaims[0].reason, ReclaimReason::Released);
    let db = &daemon.handle.state().db;
    let lease = db.lease(lease_id(&ws).parse().unwrap()).unwrap();
    assert_eq!(lease.end_reason, Some(LeaseEnd::Released));
    assert_eq!(
        db.workspace(id_of(&ws)).unwrap().state,
        WorkspaceState::Released
    );
    let log = op_log(&repo.root).await;
    assert_eq!(
        log[0].description,
        format!("reef: release workspace {}", ws.id)
    );
    let inspected = inspect(&daemon.client, &ws).await;
    assert_eq!(inspected.lease.unwrap().end_reason, "released");
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn empty_wc_commit_is_abandoned() {
    let (daemon, repo) = fixture().await;
    let empty = create_with(&daemon, &repo, 10_000).await;
    assert!(is_visible_head(&repo.root, &empty.commit_id).await);
    daemon
        .client
        .release_workspace(&repo.id, &empty.id, true, false)
        .await
        .unwrap();
    assert!(
        !is_visible_head(&repo.root, &empty.commit_id).await,
        "an empty working-copy commit is abandoned"
    );

    let written = create_with(&daemon, &repo, 10_000).await;
    std::fs::write(Path::new(&written.path).join("kept.txt"), "kept\n").unwrap();
    daemon
        .client
        .release_workspace(&repo.id, &written.id, true, false)
        .await
        .unwrap();
    let commit = daemon
        .handle
        .state()
        .db
        .workspace(id_of(&written))
        .unwrap()
        .commit_id;
    assert!(
        is_visible_head(&repo.root, &commit).await,
        "a non-empty one stays visible"
    );
}

// Shutdown (README, Shutdown): dirty workspaces get a `shutdown` snapshot
// and their leases are left as they are.
#[tokio::test(flavor = "multi_thread")]
async fn shutdown_snapshots_dirty_workspaces() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        configure(config);
        let watcher = &mut config.file.watcher;
        // Events mark the workspace dirty; no automatic snapshot runs.
        watcher.debounce = Duration::from_millis(100);
        watcher.min_interval = Duration::from_hours(1);
        watcher.fallback_interval = Duration::from_hours(2);
    })
    .await;
    let ws = create_with(&daemon, &repo, 10_000).await;
    let id = id_of(&ws);
    std::fs::write(Path::new(&ws.path).join("unsaved.txt"), "unsaved\n").unwrap();
    wait_for(Duration::from_secs(10), "workspace dirty", || async {
        daemon
            .handle
            .state()
            .manager
            .scheduler()
            .dirty()
            .contains(&id)
    })
    .await;

    let daemon = daemon.restart().await;
    let shutdown = format!("reef: final snapshot of workspace {} (shutdown)", ws.id);
    assert!(
        op_log(&repo.root)
            .await
            .iter()
            .any(|op| op.description == shutdown),
        "a shutdown snapshot was recorded"
    );
    let commit = daemon.handle.state().db.workspace(id).unwrap().commit_id;
    assert!(tree_has(&repo.root, &commit, "unsaved.txt").await);
    let lease = daemon
        .handle
        .state()
        .db
        .active_lease(id)
        .unwrap()
        .expect("the lease survives");
    assert_eq!(lease.id.as_hex(), lease_id(&ws));
}
