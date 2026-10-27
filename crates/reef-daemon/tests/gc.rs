//! Garbage collection end to end (R1.17): the jj step, base cache
//! eviction, ended workspace rows, the maintenance lock, and the working
//! copies GC rebinds. Numbered as the brief numbers them. The brief's test
//! configuration: `gc.undo_window = "0s"`, `base_cache.max_entries = 1`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{SyntheticRepo, TestDaemon, test_settings};
use jj_lib::backend::CommitId;
use jj_lib::repo::Repo as _;
use jj_lib::repo_path::RepoPathBuf;
use reef_api::v1::{
    Attention, CreateWorkspaceRequest, GcReport, Workspace, create_workspace_request,
};
use reef_daemon::config::Config;
use reef_daemon::model::WorkspaceId;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::Repo;
use reef_lib::jj::revset::RevisionError;
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";

/// The brief's GC settings, plus whatever `configure` changes.
async fn fixture_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SyntheticRepo) {
    // `TEST_LOG=reef_daemon=debug` shows the daemons' logs.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("TEST_LOG"))
        .with_test_writer()
        .try_init();
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.gc.undo_window = Duration::ZERO;
        config.file.base_cache.max_entries = 1;
        configure(config);
    })
    .await;
    // Explicit snapshots only, so each test knows its commits.
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
}

async fn create(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    start: Option<create_workspace_request::Start>,
) -> Workspace {
    daemon
        .client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.id.clone(),
            start,
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
    std::fs::write(Path::new(&ws.path).join(path), contents).unwrap();
}

/// An explicit snapshot; returns the workspace's commit afterwards.
async fn snapshot(daemon: &TestDaemon, repo: &SyntheticRepo, ws: &Workspace) -> String {
    daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap()
        .commit_id
}

async fn release(daemon: &TestDaemon, repo: &SyntheticRepo, ws: &Workspace) {
    daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
}

async fn gc(daemon: &TestDaemon, repo: &SyntheticRepo, dry_run: bool) -> GcReport {
    let mut reports = daemon
        .client
        .collect_garbage(&repo.id, dry_run)
        .await
        .unwrap()
        .reports;
    assert_eq!(reports.len(), 1);
    reports.remove(0)
}

/// `git gc --prune=@<seconds>` keeps what was written in the current
/// second; wait it out so `undo_window = 0s` covers everything so far.
async fn past_this_second() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

/// The registered root's own jj workspace, opened fresh at head (a loaded
/// store caches the commits it has read), off the runtime.
async fn open(root: &Path) -> Repo {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(Repo::open(&root, &test_settings())).unwrap()
    })
    .await
    .unwrap()
}

async fn has_commit(root: &Path, commit: &str) -> bool {
    let repo = open(root).await;
    let id = CommitId::try_from_hex(commit).unwrap();
    repo.head().store().get_commit(&id).is_ok()
}

async fn op_count(root: &Path) -> usize {
    let repo = open(root).await;
    tokio::task::spawn_blocking(move || pollster::block_on(repo.op_log(10_000)).unwrap().len())
        .await
        .unwrap()
}

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

fn bases_dir(daemon: &TestDaemon, repo: &SyntheticRepo) -> PathBuf {
    daemon
        .state_dir()
        .join("repos")
        .join(&repo.id)
        .join("bases")
}

/// The APFS probe's file in `bases/` (R1.08), which is not an entry.
const PROBE_FILE: &str = ".probe";

/// The base cache entries on disk: tree directories, without their
/// `.state` siblings or the probe file.
fn base_dirs(daemon: &TestDaemon, repo: &SyntheticRepo) -> BTreeSet<String> {
    std::fs::read_dir(bases_dir(daemon, repo))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| {
            name != PROBE_FILE && Path::new(name).extension().is_none_or(|ext| ext != "state")
        })
        .collect()
}

/// A workspace snapshotted twice, so its first commit is hidden, then
/// released. Returns both commits.
async fn hidden_snapshot(daemon: &TestDaemon, repo: &SyntheticRepo) -> (String, String) {
    let ws = create(daemon, repo, None).await;
    write(&ws, "a.txt", &"first\n".repeat(1000));
    let c1 = snapshot(daemon, repo, &ws).await;
    write(&ws, "b.txt", &"second\n".repeat(1000));
    let c2 = snapshot(daemon, repo, &ws).await;
    assert_ne!(c1, c2);
    release(daemon, repo, &ws).await;
    (c1, c2)
}

/// Three workspaces on three trees: `a` on the default start, `b` a new
/// change on `a`'s snapshot, `c` a new change on `b`'s. Each base is the
/// tree it started from, so three base entries.
async fn three_trees(daemon: &TestDaemon, repo: &SyntheticRepo) -> [Workspace; 3] {
    let a = create(daemon, repo, None).await;
    write(&a, "from-a.txt", "a\n");
    snapshot(daemon, repo, &a).await;
    let b = create(
        daemon,
        repo,
        Some(create_workspace_request::Start::NewOn(a.change_id.clone())),
    )
    .await;
    write(&b, "from-b.txt", "b\n");
    snapshot(daemon, repo, &b).await;
    let c = create(
        daemon,
        repo,
        Some(create_workspace_request::Start::NewOn(b.change_id.clone())),
    )
    .await;
    [a, b, c]
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn hidden_snapshot_is_pruned() {
    let (daemon, repo) = fixture().await;
    let (c1, c2) = hidden_snapshot(&daemon, &repo).await;
    assert!(has_commit(&repo.root, &c1).await);
    past_this_second().await;

    let report = gc(&daemon, &repo, false).await;
    assert_eq!(report.jj_error, "", "{report:?}");
    assert!(
        !has_commit(&repo.root, &c1).await,
        "the hidden snapshot is pruned"
    );
    assert!(
        has_commit(&repo.root, &c2).await,
        "the released workspace's commit stays a visible head"
    );
    assert_eq!(op_count(&repo.root).await, 2, "the head and the root");
    assert!(
        report.jj_bytes_after < report.jj_bytes_before,
        "{} -> {}",
        report.jj_bytes_before,
        report.jj_bytes_after
    );
    assert!(!report.dry_run);
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn undo_window_protects() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.undo_window = Duration::from_hours(1);
    })
    .await;
    let (c1, c2) = hidden_snapshot(&daemon, &repo).await;
    let ops_before = op_count(&repo.root).await;
    past_this_second().await;

    let report = gc(&daemon, &repo, false).await;
    assert_eq!(report.jj_error, "", "{report:?}");
    assert!(has_commit(&repo.root, &c1).await, "inside the undo window");
    assert!(has_commit(&repo.root, &c2).await);
    assert_eq!(op_count(&repo.root).await, ops_before, "nothing abandoned");
    assert_eq!(
        report.workspace_rows_removed, 0,
        "released within the window"
    );
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn base_cache_evicts_unreferenced() {
    let (daemon, repo) = fixture().await;
    let [a, b, c] = three_trees(&daemon, &repo).await;
    let before = base_dirs(&daemon, &repo);
    assert_eq!(before.len(), 3, "{before:?}");
    release(&daemon, &repo, &a).await;
    release(&daemon, &repo, &b).await;
    let kept = daemon
        .handle
        .state()
        .db
        .workspace(id_of(&c))
        .unwrap()
        .base_tree_id;

    let report = gc(&daemon, &repo, false).await;
    assert_eq!(report.base_entries_removed, 2, "{report:?}");
    assert!(report.base_bytes_freed > 0);
    let after: Vec<String> = std::fs::read_dir(bases_dir(&daemon, &repo))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != PROBE_FILE)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    assert_eq!(
        after,
        [kept.clone(), format!("{kept}.state")],
        "no leftovers"
    );
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn active_base_is_never_evicted() {
    let (daemon, repo) = fixture().await;
    let a = create(&daemon, &repo, None).await;
    write(&a, "from-a.txt", "a\n");
    snapshot(&daemon, &repo, &a).await;
    let _b = create(
        &daemon,
        &repo,
        Some(create_workspace_request::Start::NewOn(a.change_id.clone())),
    )
    .await;
    let before = base_dirs(&daemon, &repo);
    assert_eq!(before.len(), 2);

    let report = gc(&daemon, &repo, false).await;
    assert_eq!(report.base_entries_removed, 0, "{report:?}");
    assert_eq!(
        base_dirs(&daemon, &repo),
        before,
        "both stay, above max_entries = 1"
    );
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn reclaimed_rows_are_deleted() {
    let (daemon, repo) = fixture_with(|config| {
        let lease = &mut config.file.lease;
        lease.default = Duration::from_secs(1);
        lease.grace = Duration::from_millis(500);
        lease.tick = Duration::from_millis(200);
        lease.heartbeat = Duration::from_millis(300);
    })
    .await;
    let ws = create(&daemon, &repo, None).await;
    let state = daemon.handle.state();
    let begun = Instant::now();
    while state.db.workspace(id_of(&ws)).unwrap().state.as_str() != "reclaimed" {
        assert!(begun.elapsed() < Duration::from_secs(15), "not reclaimed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    // A state directory left behind, which GC removes with the row.
    let wc = daemon
        .state_dir()
        .join("repos")
        .join(&repo.id)
        .join("wc")
        .join(&ws.id);
    std::fs::create_dir_all(&wc).unwrap();
    std::fs::write(wc.join("checkout"), "left behind").unwrap();
    let repo_id = repo.id.parse().unwrap();
    let reclaims_before = state.db.reclaims(Some(repo_id), 10).unwrap().len();
    past_this_second().await;

    let report = gc(&daemon, &repo, false).await;
    assert_eq!(report.workspace_rows_removed, 1, "{report:?}");
    assert!(report.wc_bytes_freed > 0);
    assert!(!wc.exists());
    let error = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert_eq!(
        state.db.reclaims(Some(repo_id), 10).unwrap().len(),
        reclaims_before,
        "the reclaim log stays"
    );
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn dry_run_changes_nothing() {
    let (daemon, repo) = fixture().await;
    let (c1, _) = hidden_snapshot(&daemon, &repo).await;
    let ops_before = op_count(&repo.root).await;
    let bases_before = base_dirs(&daemon, &repo);
    past_this_second().await;

    let report = gc(&daemon, &repo, true).await;
    assert!(report.dry_run);
    assert!(has_commit(&repo.root, &c1).await);
    assert_eq!(op_count(&repo.root).await, ops_before);
    assert_eq!(base_dirs(&daemon, &repo), bases_before);
    assert_eq!(report.jj_bytes_before, report.jj_bytes_after);
    assert!(report.jj_bytes_before > 0);
    // What a real run would remove: the released workspace's row and base.
    assert_eq!(report.workspace_rows_removed, 1, "{report:?}");
    assert_eq!(report.base_entries_removed, 1);
    assert!(report.base_bytes_freed > 0);
    assert_eq!(
        daemon
            .client
            .list_workspaces(reef_api::v1::ListWorkspacesRequest::default())
            .await
            .unwrap()
            .len(),
        1,
        "the row is still there"
    );
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn gc_refuses_during_maintenance() {
    let (daemon, repo) = fixture().await;
    let held = daemon.handle.state().maintenance.try_acquire().unwrap();
    let error = daemon
        .client
        .collect_garbage(&repo.id, false)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    assert!(
        error.to_string().contains("maintenance in progress"),
        "{error}"
    );
    drop(held);
    gc(&daemon, &repo, true).await;
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn missing_git_reports_and_continues() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.git_executable = "/nonexistent/git".to_owned();
    })
    .await;
    hidden_snapshot(&daemon, &repo).await;
    past_this_second().await;

    let report = gc(&daemon, &repo, false).await;
    assert!(!report.jj_error.is_empty(), "{report:?}");
    assert!(report.jj_error.contains("git gc"), "{}", report.jj_error);
    assert_eq!(report.base_entries_removed, 1, "the base cache step ran");
    assert_eq!(report.workspace_rows_removed, 1, "the workspace step ran");
    assert_eq!(
        op_count(&repo.root).await,
        2,
        "operations were abandoned before git gc ran"
    );
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn two_workspaces_same_change_keep_both_sides() {
    let (daemon, repo) = fixture().await;
    let a = create(&daemon, &repo, None).await;
    let b = create(
        &daemon,
        &repo,
        Some(create_workspace_request::Start::Change(a.change_id.clone())),
    )
    .await;
    write(&a, "from-a.txt", "a\n");
    write(&b, "from-b.txt", "b\n");
    let ca = snapshot(&daemon, &repo, &a).await;
    let cb = snapshot(&daemon, &repo, &b).await;
    assert_ne!(ca, cb);
    past_this_second().await;

    let report = gc(&daemon, &repo, false).await;
    assert_eq!(report.jj_error, "", "{report:?}");
    assert!(has_commit(&repo.root, &ca).await);
    assert!(has_commit(&repo.root, &cb).await);
    let error = open(&repo.root)
        .await
        .resolve_revision(&a.change_id)
        .await
        .unwrap_err();
    let RevisionError::Ambiguous {
        commit_ids: commits,
        ..
    } = error
    else {
        panic!("expected a divergent change, got {error:?}");
    };
    let commits: BTreeSet<String> = commits.iter().map(ToString::to_string).collect();
    assert_eq!(commits, BTreeSet::from([ca, cb]));
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn active_workspace_snapshots_after_gc() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, None).await;
    write(&ws, "before-gc.txt", "before\n");
    snapshot(&daemon, &repo, &ws).await;
    // Unsnapshotted when GC starts; GC's own snapshot takes it.
    write(&ws, "during-gc.txt", "during\n");
    past_this_second().await;

    let report = gc(&daemon, &repo, false).await;
    assert_eq!(report.jj_error, "", "{report:?}");
    assert_eq!(op_count(&repo.root).await, 2);
    let entry = daemon.handle.state().manager.entry(id_of(&ws)).unwrap();
    let wc_op = entry
        .jj
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .working_copy_op_id();
    assert_eq!(
        wc_op,
        open(&repo.root).await.op_id(),
        "rebound to the GC head"
    );

    write(&ws, "after-gc.txt", "after\n");
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(snapshot.changed);
    for path in ["before-gc.txt", "during-gc.txt", "after-gc.txt"] {
        assert!(
            tree_has(&repo.root, &snapshot.commit_id, path).await,
            "{path}"
        );
    }
    let row = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert!(!row.divergent);
    assert_eq!(row.attention(), Attention::Unspecified);
    assert_eq!(op_count(&repo.root).await, 3, "one snapshot on the GC head");
}

// A GC cut short (here: one run behind the stopped daemon's back, which
// rebinds nothing) leaves the workspace's working copy on an abandoned
// operation; the next start moves it to the head.
#[tokio::test(flavor = "multi_thread")]
async fn stranded_working_copy_is_repaired_at_start() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, None).await;
    write(&ws, "before.txt", "before\n");
    snapshot(&daemon, &repo, &ws).await;
    let TestDaemon {
        handle,
        client,
        tmp,
        config,
    } = daemon;
    drop(client);
    handle.shutdown();
    handle.join().await.unwrap();
    past_this_second().await;
    let root = repo.root.clone();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let mut main = Repo::open(&root, &test_settings()).await.unwrap();
            main.gc(std::time::SystemTime::now()).await.unwrap();
        });
    })
    .await
    .unwrap();

    let handle = reef_daemon::Daemon::start(config.clone()).await.unwrap();
    let client = reef_daemon::client::Client::connect(handle.socket_path())
        .await
        .unwrap();
    let daemon = TestDaemon {
        handle,
        client,
        tmp,
        config,
    };
    daemon.handle.state().manager.scheduler().pause_watcher();
    write(&ws, "after.txt", "after\n");
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(snapshot.changed);
    assert!(tree_has(&repo.root, &snapshot.commit_id, "before.txt").await);
    assert!(tree_has(&repo.root, &snapshot.commit_id, "after.txt").await);
    let row = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert!(!row.divergent);
    assert_eq!(
        row.attention(),
        Attention::Unspecified,
        "not recovered onto a new change"
    );
}

/// The first regular file at least three directories deep in `root`, by
/// name: a file an agent would edit.
fn deep_file(root: &Path) -> PathBuf {
    let mut dir = root.to_owned();
    for _ in 0..3 {
        let mut subdirs: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| path.is_dir())
            .collect();
        subdirs.sort();
        match subdirs.into_iter().next() {
            Some(subdir) => dir = subdir,
            None => break,
        }
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file())
        .collect();
    files.sort();
    files.into_iter().next().unwrap()
}

// The acceptance criterion's measurement: after 1,000 snapshots of an
// edited file on the medium preset and a GC with `undo_window = 0s`, the
// git objects directory is within 10% of its size after the first
// snapshot. Explicit snapshots stand in for watcher ones (the operations
// are the same; only the index's `kind` differs). Run with
// `cargo test --release -p reef-daemon --test gc -- --ignored --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual measurement on the medium preset; prints its numbers"]
async fn medium_thousand_snapshots_then_gc() {
    let (after_first, after_gc) = thousand_snapshots_then_gc(Duration::ZERO).await;
    assert!(
        after_gc * 10 <= after_first * 11,
        "{after_gc} is not within 10% of {after_first}"
    );
}

// The control: the same with a one-hour undo window, which abandons nothing,
// so the difference from the run above is what abandoning freed rather than
// what `git gc` packing did.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual measurement on the medium preset; prints its numbers"]
async fn medium_thousand_snapshots_then_gc_control() {
    thousand_snapshots_then_gc(Duration::from_hours(1)).await;
}

/// 1,000 snapshots of one edited file on the medium preset, then a GC with
/// `undo_window`; prints the numbers and returns the git objects
/// directory's size after the first snapshot and after GC.
#[allow(
    clippy::print_stderr,
    reason = "a manual measurement prints its numbers"
)]
async fn thousand_snapshots_then_gc(undo_window: Duration) -> (u64, u64) {
    const SNAPSHOTS: usize = 1000;
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Medium, |config| {
        config.file.gc.undo_window = undo_window;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    let ws = create(&daemon, &repo, None).await;
    let file = deep_file(Path::new(&ws.path));
    let objects = repo.root.join(".jj/repo/store/git/objects");
    let size = |path: &Path| reef_daemon::backend::fs_util::allocated_bytes(path).unwrap();

    let mut contents = std::fs::read_to_string(&file).unwrap_or_default();
    let begun = Instant::now();
    let mut after_first = 0;
    for i in 0..SNAPSHOTS {
        writeln!(contents, "edit {i}").unwrap();
        std::fs::write(&file, &contents).unwrap();
        snapshot(&daemon, &repo, &ws).await;
        if i == 0 {
            after_first = size(&objects);
        }
    }
    let snapshotting = begun.elapsed();
    let before_gc = size(&objects);
    let ops_before = op_count(&repo.root).await;
    past_this_second().await;

    let begun = Instant::now();
    let report = gc(&daemon, &repo, false).await;
    let gc_time = begun.elapsed();
    let after_gc = size(&objects);
    let ops_after = op_count(&repo.root).await;
    #[allow(clippy::cast_precision_loss, reason = "printed to one decimal")]
    let mib = |bytes: u64| bytes as f64 / f64::from(1 << 20);
    eprintln!(
        "medium preset, undo window {undo_window:?}, {SNAPSHOTS} snapshots of {} ({snapshotting:.1?})",
        file.display()
    );
    eprintln!("operations: {ops_before} before GC, {ops_after} after");
    eprintln!(
        "git objects: {:.1} MiB after the first snapshot, {:.1} MiB before GC, {:.1} MiB after",
        mib(after_first),
        mib(before_gc),
        mib(after_gc)
    );
    eprintln!(
        "jj step: {:.1} MiB -> {:.1} MiB; index rebuild {} ms; GC {gc_time:.1?} in all",
        mib(report.jj_bytes_before),
        mib(report.jj_bytes_after),
        report.reindex_ms
    );
    assert_eq!(report.jj_error, "", "{report:?}");
    (after_first, after_gc)
}

// R2.09 16: a subscription caught up when GC rewrites the log moves to the
// rewritten head and goes on; a cursor from before the rewrite cannot
// resume.
#[tokio::test(flavor = "multi_thread")]
async fn gc_keeps_caught_up_subscriptions() {
    use reef_api::v1::{
        OperationKind, SubscribeOperationsRequest, subscribe_operations_request::Start,
    };
    use reef_daemon::client::ClientError;

    let (daemon, repo) = fixture().await;
    // No active workspace: GC records nothing before its rewrite, so the
    // subscription is caught up when it begins.
    let ws = create(&daemon, &repo, None).await;
    release(&daemon, &repo, &ws).await;
    let request = SubscribeOperationsRequest {
        repos: vec![repo.id.clone()],
        ..SubscribeOperationsRequest::default()
    };
    let mut stream = daemon
        .client
        .subscribe_operations(request.clone())
        .await
        .unwrap();
    let first = stream.message().await.unwrap().unwrap();
    assert!(first.live);
    let before = first.cursor;
    past_this_second().await;
    // R2.22, 13: the subscription's ring is primed before GC.
    let primed = Instant::now();
    while daemon.handle.state().broker.stats().ingest_reads == 0 {
        assert!(
            primed.elapsed() < Duration::from_secs(5),
            "the ring is primed"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let broker_before = daemon.handle.state().broker.stats();

    let report = gc(&daemon, &repo, false).await;
    assert_eq!(report.jj_error, "", "{report:?}");
    assert!(op_count(&repo.root).await <= 2, "the log was cut");
    let head = open(&repo.root).await.op_id().to_string();

    let ws = create(&daemon, &repo, None).await;
    let created = loop {
        let frame = tokio::time::timeout(Duration::from_secs(5), stream.message())
            .await
            .expect("a frame in time")
            .unwrap()
            .unwrap();
        if let Some(op) = frame.operations.first() {
            break op.clone();
        }
    };
    assert_eq!(created.parent_ids, vec![head], "nothing from the rewrite");
    assert_eq!(created.kind(), OperationKind::CreateWorkspace);
    assert_eq!(created.workspace_id, ws.id);
    // R2.22, 13: the ring restarted at the rewritten head, where the
    // caught-up subscription moved, so it never read the log.
    let broker_after = daemon.handle.state().broker.stats();
    assert_eq!(broker_after.resets, broker_before.resets + 1);
    assert_eq!(broker_after.log_reads, broker_before.log_reads);

    let error = daemon
        .client
        .subscribe_operations(SubscribeOperationsRequest {
            start: Some(Start::Cursor(before)),
            ..request
        })
        .await
        .unwrap_err();
    let ClientError::Rpc { status, reason, .. } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(reason, "OPERATION_LOG_REWRITTEN");
}

// R2.09, review: with an undo window GC keeps the old copies of the
// operations it rewrites and collects what is older. A subscription that
// has not read up to the rewritten head (GC's own snapshot lands just
// before it) reads up to it and goes on from its rewrite, and a cursor
// from before GC is OPERATION_LOG_REWRITTEN, not an I/O error from the
// collected history behind it.
#[tokio::test(flavor = "multi_thread")]
async fn gc_with_an_undo_window_keeps_subscriptions() {
    use reef_api::v1::{SubscribeOperationsRequest, subscribe_operations_request::Start};
    use reef_daemon::client::ClientError;

    let (daemon, repo) =
        fixture_with(|config| config.file.gc.undo_window = Duration::from_secs(2)).await;
    let ws = create(&daemon, &repo, None).await;
    // Everything so far falls outside the window; what follows inside it.
    tokio::time::sleep(Duration::from_millis(2500)).await;
    write(&ws, "inside.txt", "inside\n");
    snapshot(&daemon, &repo, &ws).await;
    let request = SubscribeOperationsRequest {
        repos: vec![repo.id.clone()],
        ..SubscribeOperationsRequest::default()
    };
    let mut stream = daemon
        .client
        .subscribe_operations(request.clone())
        .await
        .unwrap();
    let before = stream.message().await.unwrap().unwrap().cursor;
    // Unsnapshotted when GC starts: GC's own snapshot takes it, just before
    // the rewrite.
    write(&ws, "during-gc.txt", "during\n");

    let report = gc(&daemon, &repo, false).await;
    assert_eq!(report.jj_error, "", "{report:?}");
    let head = open(&repo.root).await.op_id().to_string();
    write(&ws, "after-gc.txt", "after\n");
    snapshot(&daemon, &repo, &ws).await;
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    let after = loop {
        let frame = tokio::time::timeout(deadline - Instant::now(), stream.message())
            .await
            .expect("a frame in time")
            .unwrap_or_else(|status| panic!("{status:?} after {seen:?}"))
            .unwrap();
        seen.extend(frame.operations.iter().map(|op| op.description.clone()));
        if let Some(op) = frame
            .operations
            .iter()
            .find(|op| op.parent_ids == [head.clone()])
        {
            break op.clone();
        }
    };
    assert_eq!(after.workspace_id, ws.id);
    assert!(
        seen.iter()
            .any(|description| description.contains("snapshot")),
        "GC's snapshot arrives: {seen:?}"
    );

    let error = daemon
        .client
        .subscribe_operations(SubscribeOperationsRequest {
            start: Some(Start::Cursor(before)),
            ..request
        })
        .await
        .unwrap_err();
    let ClientError::Rpc { status, reason, .. } = &error else {
        panic!("{error:?}");
    };
    assert_eq!(
        (status.code(), reason.as_str()),
        (tonic::Code::FailedPrecondition, "OPERATION_LOG_REWRITTEN"),
        "{error:?}"
    );
}
