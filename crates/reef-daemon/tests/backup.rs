//! Local backup and restore end to end (R1.18, ADR 0020): a backup taken
//! while agents write restores to a daemon that serves the same changes and
//! operation log, with no workspace lost and no lease reclaimed on the
//! first tick; quiesce and resume; the refusal rules; colocated
//! repositories; restoring to a new root; and GC's retention floor.
//! Numbered as the brief numbers them. The brief's test configuration:
//! `lease.default = 30s`, `restart_grace = 5s`, `tick = 500ms`,
//! `watcher.debounce = 100ms`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime};

use common::{SyntheticRepo, TestDaemon, test_settings};
use jj_lib::backend::CommitId;
use jj_lib::repo::Repo as _;
use reef_daemon::client::json::millis_of;
use reef_daemon::config::Config;
use reef_daemon::db::META_REMATERIALIZE_PENDING;
use reef_daemon::maintenance::backup::{HEADS_FILE, MANIFEST_FILE, Manifest, read_op_heads};
use reef_daemon::maintenance::restore::{RestoreError, RestoreOptions, RestoreReport, restore};
use reef_daemon::model::{RepoId, Timestamp};
use reef_daemon::proto::v0::register_request::IfNotJj;
use reef_daemon::proto::v0::{
    BackupProgress, CreateRequest, ListWorkspacesRequest, Workspace, create_request,
};
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::{CommitIdHex, Repo, RepoPath};
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";

/// The brief's settings, plus whatever `configure` changes.
async fn fixture_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SyntheticRepo) {
    logs();
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        brief_config(config);
        configure(config);
    })
    .await
}

fn brief_config(config: &mut Config) {
    config.file.lease.default = Duration::from_secs(30);
    config.file.lease.restart_grace = Duration::from_secs(5);
    config.file.lease.tick = Duration::from_millis(500);
    config.file.watcher.debounce = Duration::from_millis(100);
}

/// [`fixture_with`] with explicit snapshots only, so each test knows its
/// commits.
async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = fixture_with(|_| {}).await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn create(
    daemon: &TestDaemon,
    repo: &str,
    start: Option<create_request::Start>,
) -> Workspace {
    daemon
        .client
        .create_workspace(CreateRequest {
            repo: repo.to_owned(),
            start,
            owner: OWNER.to_owned(),
            ..CreateRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

fn write(ws: &Workspace, path: &str, contents: &str) {
    std::fs::write(Path::new(&ws.path).join(path), contents).unwrap();
}

/// An explicit snapshot; returns the workspace's commit and the operation.
async fn snapshot(daemon: &TestDaemon, repo: &str, ws: &Workspace) -> (String, String) {
    let response = daemon
        .client
        .snapshot_workspace(repo, &ws.id, "")
        .await
        .unwrap();
    (response.commit_id, response.op_id)
}

async fn workspaces(daemon: &TestDaemon, repo: &str) -> Vec<Workspace> {
    daemon
        .client
        .list_workspaces(ListWorkspacesRequest {
            repo: repo.to_owned(),
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap()
}

/// A backup of `repo` (empty for every repository) into `dest`: its
/// progress messages and its manifest.
async fn backup(daemon: &TestDaemon, dest: &Path, repo: &str) -> (Vec<BackupProgress>, Manifest) {
    let mut stream = daemon.client.backup(dest, repo).await.unwrap();
    let mut steps = Vec::new();
    while let Some(step) = stream.message().await.unwrap() {
        steps.push(step);
    }
    let last = steps.last().expect("a last message");
    assert!(last.done, "{steps:?}");
    assert_eq!(Path::new(&last.manifest_path), dest.join(MANIFEST_FILE));
    (steps, Manifest::read(dest).unwrap())
}

/// Restore options for a state directory, from `config`'s lease settings.
fn options(config: &Config, into: &Path) -> RestoreOptions {
    RestoreOptions {
        into: into.to_owned(),
        repo: None,
        root: None,
        force: false,
        lease: config.file.lease.clone(),
        settings: test_settings(),
    }
}

async fn run_restore(src: &Path, opts: RestoreOptions) -> Result<RestoreReport, RestoreError> {
    let src = src.to_owned();
    tokio::task::spawn_blocking(move || restore(&src, &opts))
        .await
        .unwrap()
}

/// The repository at `root`, opened fresh at head, off the runtime.
async fn open(root: &Path) -> Repo {
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(Repo::open(&root, &test_settings())).unwrap()
    })
    .await
    .unwrap()
}

/// `path` in `commit`'s tree, as text.
async fn file_at(root: &Path, commit: &str, path: &str) -> Option<String> {
    let repo = open(root).await;
    let (commit, path) = (commit.parse::<CommitIdHex>().unwrap(), path.to_owned());
    tokio::task::spawn_blocking(move || {
        let path = RepoPath::from_internal_string(&path).unwrap();
        pollster::block_on(repo.read_file_at(&commit, path))
            .unwrap()
            .map(|bytes| String::from_utf8(bytes).unwrap())
    })
    .await
    .unwrap()
}

async fn has_commit(root: &Path, commit: &str) -> bool {
    let repo = open(root).await;
    let id = CommitId::try_from_hex(commit).unwrap();
    repo.head().store().get_commit(&id).is_ok()
}

/// The operation ids of the whole log at `root`'s head.
async fn op_log(root: &Path) -> BTreeSet<String> {
    let repo = open(root).await;
    tokio::task::spawn_blocking(move || {
        pollster::block_on(repo.op_log(100_000))
            .unwrap()
            .into_iter()
            .map(|entry| entry.id.to_string())
            .collect()
    })
    .await
    .unwrap()
}

fn heads_json(dest: &Path, repo: &str) -> Vec<String> {
    let text = std::fs::read_to_string(dest.join("repos").join(repo).join(HEADS_FILE)).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn live_heads(root: &Path) -> Vec<String> {
    read_op_heads(&root.join(".jj/repo"))
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect()
}

/// Removes `path` as a lost machine would: on Linux, overlay workspaces
/// stay mounted after their daemon stops (R1.09), so the mounts under it go
/// first, as a reboot would take them.
fn remove(path: &Path) {
    #[cfg(target_os = "linux")]
    unmount_under(path);
    match std::fs::remove_dir_all(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => {
            std::fs::remove_file(path).unwrap();
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("remove {}: {error}", path.display()),
    }
}

/// Unmounts every mount at or under `path`, deepest first.
#[cfg(target_os = "linux")]
fn unmount_under(path: &Path) {
    let Ok(path) = std::fs::canonicalize(path) else {
        return;
    };
    let mounts = std::fs::read_to_string("/proc/self/mounts").unwrap();
    let mut under: Vec<(String, String)> = mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let (point, fstype) = (fields.nth(1)?, fields.next()?);
            Path::new(point)
                .starts_with(&path)
                .then(|| (point.to_owned(), fstype.to_owned()))
        })
        .collect();
    under.sort_by_key(|(point, _)| std::cmp::Reverse(point.len()));
    for (point, fstype) in under {
        let (program, args) = if fstype.starts_with("fuse") {
            ("fusermount3", vec!["-uz", point.as_str()])
        } else {
            ("umount", vec!["-l", point.as_str()])
        };
        let output = std::process::Command::new(program)
            .args(&args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{program} {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

/// `git gc --prune=@<seconds>` keeps what was written in the current
/// second; wait it out so `undo_window = 0s` covers everything so far.
async fn past_this_second() {
    tokio::time::sleep(Duration::from_millis(1100)).await;
}

/// `lines` lines `0`, `1`, ... as the writers write them.
fn lines(count: usize) -> String {
    (0..count).fold(String::new(), |mut text, line| {
        let _ = writeln!(text, "line {line}");
        text
    })
}

/// Appends `line N` to `load.txt` in `dir` every 50 ms until `stop`;
/// returns how many lines it wrote.
fn spawn_writer(dir: PathBuf, stop: Arc<AtomicBool>) -> tokio::task::JoinHandle<usize> {
    tokio::spawn(async move {
        let mut written = 0;
        while !stop.load(Ordering::Relaxed) {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("load.txt"))
                .unwrap();
            writeln!(file, "line {written}").unwrap();
            written += 1;
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        written
    })
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn round_trip_under_load() {
    // The watcher stays on: snapshots run throughout the backup.
    let (daemon, repo) = fixture_with(|_| {}).await;
    let mut created = Vec::new();
    for _ in 0..5 {
        created.push(create(&daemon, &repo.id, None).await);
    }
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = created
        .iter()
        .map(|ws| spawn_writer(PathBuf::from(&ws.path), Arc::clone(&stop)))
        .collect();
    // At least a second of writes, and a watcher snapshot of each workspace
    // (the first waits out `watcher.min_interval`), before the backup.
    tokio::time::sleep(Duration::from_secs(1)).await;
    let deadline = Instant::now() + Duration::from_secs(30);
    while workspaces(&daemon, &repo.id)
        .await
        .iter()
        .any(|ws| ws.last_snapshot_op.is_empty())
    {
        assert!(Instant::now() < deadline, "every workspace snapshots");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Every workspace's latest snapshot so far: the backup holds at least
    // these.
    let before: Vec<String> = workspaces(&daemon, &repo.id)
        .await
        .into_iter()
        .map(|ws| ws.last_snapshot_op)
        .collect();
    let dest = daemon.tmp.path().join("backup");
    let (steps, manifest) = backup(&daemon, &dest, "").await;
    assert!(manifest.complete);
    assert_eq!(manifest.repos.len(), 1);
    assert!(
        steps.iter().any(|step| step.step == "operations"),
        "{steps:?}"
    );
    let pause_ms = manifest.repos[0].pause_ms;
    eprintln!("pause under load on the small preset: {pause_ms} ms");
    let heads = heads_json(&dest, &repo.id);
    assert_eq!(heads, manifest.repos[0].op_heads);

    tokio::time::sleep(Duration::from_secs(1)).await;
    stop.store(true, Ordering::Relaxed);
    let mut written = std::collections::HashMap::new();
    for (ws, writer) in created.iter().zip(writers) {
        written.insert(ws.id.clone(), writer.await.unwrap());
    }
    let stopped = daemon.stop_keeping().await;
    let state_dir = stopped.config.paths.state_dir.clone();
    remove(&repo.root.join(".jj/repo"));
    remove(&state_dir.join("repos"));
    remove(&state_dir.join("reef.sqlite3"));

    let mut opts = options(&stopped.config, &state_dir);
    opts.force = true;
    let report = run_restore(&dest, opts).await.unwrap();
    assert_eq!(report.workspaces_kept, 5);
    assert_eq!(
        live_heads(&repo.root),
        heads,
        "the op heads are the backup's"
    );
    let restored_log = op_log(&repo.root).await;
    for op in &before {
        assert!(restored_log.contains(op), "the backup has snapshot {op}");
    }

    let started = Timestamp::now();
    let daemon = stopped.start().await;
    let repos = daemon.client.list_repositories().await.unwrap();
    assert_eq!(repos.len(), 1);
    assert_eq!(repos[0].id, repo.id);
    let restored = workspaces(&daemon, &repo.id).await;
    assert_eq!(restored.len(), 5, "{restored:?}");
    for ws in &restored {
        check_restored(&repo.root, ws, written[&ws.id], started).await;
    }
    let reclaimed = daemon
        .handle
        .state()
        .manager
        .leases()
        .tick_once()
        .await
        .unwrap();
    assert!(
        reclaimed.is_empty(),
        "the first tick reclaims nothing: {reclaimed:?}"
    );

    for ws in &restored {
        write(ws, "after.txt", "after the restore\n");
        let (commit, _) = snapshot(&daemon, &repo.id, ws).await;
        assert_ne!(commit, ws.commit_id);
        assert_eq!(
            file_at(&repo.root, &commit, "after.txt").await.as_deref(),
            Some("after the restore\n")
        );
    }
}

/// One workspace of [`round_trip_under_load`] after the restore: active,
/// its lease past the restart grace, on its commit in the restored view,
/// holding some of what its writer wrote, in order, on disk too.
async fn check_restored(root: &Path, ws: &Workspace, written: usize, started: Timestamp) {
    assert_eq!(ws.state, "active", "{ws:?}");
    let expires = millis_of(ws.lease.as_ref().unwrap().expires_at.as_ref().unwrap());
    assert!(
        expires >= started.0 + 5_000,
        "the lease has the restart grace: {expires} < {} + 5000",
        started.0
    );
    let in_view = open(root)
        .await
        .workspace_commit(&format!("reef-{}", ws.id))
        .unwrap();
    assert_eq!(
        in_view.commit_id.as_str(),
        ws.commit_id,
        "the restored view's commit"
    );
    // What the backup captured: some of what was written, in order.
    let captured = file_at(root, &ws.commit_id, "load.txt")
        .await
        .unwrap_or_default();
    let count = captured.lines().count();
    assert!(
        count >= 1,
        "a second of writes was snapshotted before the backup"
    );
    assert!(count <= written);
    assert_eq!(captured, lines(count));
    let on_disk = std::fs::read_to_string(Path::new(&ws.path).join("load.txt")).unwrap();
    assert_eq!(on_disk, captured, "the directory is the commit's");
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn manifest_is_consistent() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo.id, None).await;
    for round in 0..3 {
        write(&ws, "round.txt", &format!("{round}\n"));
        snapshot(&daemon, &repo.id, &ws).await;
    }
    let dest = daemon.tmp.path().join("backup");
    let (_, manifest) = backup(&daemon, &dest, &repo.id).await;
    let entry = &manifest.repos[0];
    assert_eq!(entry.id, repo.id);
    assert!(!entry.colocated);
    assert!(entry.files > 0 && entry.bytes > 0);
    let copy = dest.join("repos").join(&repo.id).join("repo");
    for dir in ["op_store/operations", "op_store/views", "store/git/objects"] {
        assert!(copy.join(dir).is_dir(), "{dir}");
    }
    assert!(copy.join("index/type").is_file());
    assert_eq!(
        std::fs::read_dir(copy.join("index/segments"))
            .unwrap()
            .count(),
        0,
        "the index is never backed up"
    );

    // Restored elsewhere, every head's ancestry and every workspace's
    // commit is there.
    let elsewhere = daemon.tmp.path().join("elsewhere");
    let root = daemon.tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let mut opts = options(&daemon.config, &elsewhere);
    opts.root = Some(root.clone());
    opts.repo = Some(repo.id.parse::<RepoId>().unwrap());
    run_restore(&dest, opts).await.unwrap();
    let restored_log = op_log(&root).await;
    for head in &entry.op_heads {
        assert!(restored_log.contains(head), "{head}");
    }
    let db = reef_daemon::db::Db::open(&elsewhere.join("reef.sqlite3")).unwrap();
    let elsewhere = std::fs::canonicalize(&elsewhere).unwrap();
    let rows = db.workspaces(None, &[]).unwrap();
    assert_eq!(rows.len(), 1);
    for row in rows {
        assert!(has_commit(&root, &row.commit_id).await, "{}", row.commit_id);
        // Both spellings of the old state directory moved: the configured
        // `/tmp/...` of the state dir, the canonical `/private/tmp/...` of
        // the workspace path (on macOS).
        assert!(row.path.starts_with(&elsewhere), "{}", row.path.display());
        assert!(
            row.state_dir.starts_with(&elsewhere),
            "{}",
            row.state_dir.display()
        );
    }
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn incomplete_backup_is_refused() {
    let (daemon, repo) = fixture().await;
    let dest = daemon.tmp.path().join("backup");
    backup(&daemon, &dest, &repo.id).await;
    let path = dest.join(MANIFEST_FILE);
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    manifest.as_object_mut().unwrap().remove("complete");
    std::fs::write(&path, manifest.to_string()).unwrap();

    let into = daemon.tmp.path().join("restored");
    let error = run_restore(&dest, options(&daemon.config, &into))
        .await
        .unwrap_err();
    assert!(matches!(error, RestoreError::Incomplete(_)), "{error:?}");
    assert!(
        error.to_string().contains(&path.display().to_string()),
        "{error}"
    );
    assert!(!into.join("reef.sqlite3").exists(), "nothing was written");
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn newer_live_repo_is_refused() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo.id, None).await;
    let dest = daemon.tmp.path().join("backup");
    backup(&daemon, &dest, &repo.id).await;
    write(&ws, "later.txt", "after the backup\n");
    let (_, later_op) = snapshot(&daemon, &repo.id, &ws).await;
    assert_ne!(later_op, "");
    let stopped = daemon.stop_keeping().await;
    let state_dir = stopped.config.paths.state_dir.clone();

    let error = run_restore(&dest, options(&stopped.config, &state_dir))
        .await
        .unwrap_err();
    assert!(matches!(error, RestoreError::NewerLive { .. }), "{error:?}");
    assert_eq!(error.kind(), ErrorKind::Conflict);
    assert!(
        op_log(&repo.root).await.contains(&later_op),
        "nothing changed"
    );

    let mut opts = options(&stopped.config, &state_dir);
    opts.force = true;
    let report = run_restore(&dest, opts).await.unwrap();
    assert!(
        !op_log(&repo.root).await.contains(&later_op),
        "the later operation is gone"
    );
    assert_eq!(live_heads(&repo.root), heads_json(&dest, &repo.id));
    assert_eq!(
        report.replaced.len(),
        2,
        "the old database and .jj/repo: {report:?}"
    );
    for path in &report.replaced {
        assert!(path.exists(), "{} is kept", path.display());
    }

    // The directory survived, with a working copy written at the operation
    // that is gone: the workspace still snapshots, and what is on disk (the
    // edit after the backup) is its next snapshot.
    let daemon = stopped.start().await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    let restored = workspaces(&daemon, &repo.id).await;
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].state, "active");
    let (commit, _) = snapshot(&daemon, &repo.id, &restored[0]).await;
    assert_eq!(
        file_at(&repo.root, &commit, "later.txt").await.as_deref(),
        Some("after the backup\n")
    );
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn quiesce_blocks_writes_and_auto_resumes() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo.id, None).await;
    let lease = ws.lease.clone().unwrap();
    let grace_ms = i64::try_from(daemon.config.file.lease.grace.as_millis()).unwrap();
    let quiesced = daemon
        .client
        .quiesce("", Duration::from_millis(2000))
        .await
        .unwrap();
    let begun = Instant::now();
    assert_ne!(quiesced.token, "");

    write(&ws, "quiet.txt", "written while quiesced\n");
    let snapshotting = tokio::spawn({
        let client = daemon.client.clone();
        let (repo, ws) = (repo.id.clone(), ws.id.clone());
        async move { client.snapshot_workspace(&repo, &ws, "").await }
    });
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert!(
        !snapshotting.is_finished(),
        "the snapshot waits for the resume"
    );
    let error = daemon
        .client
        .heartbeat_workspace(&repo.id, &ws.id, &lease.id)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error:?}");

    tokio::time::sleep(Duration::from_millis(2500).saturating_sub(begun.elapsed())).await;
    let snapshot = tokio::time::timeout(Duration::from_secs(10), snapshotting)
        .await
        .expect("the snapshot finishes after the auto-resume")
        .unwrap()
        .unwrap();
    assert!(snapshot.changed);
    let resumed_at = millis_of(quiesced.auto_resume_at.as_ref().unwrap());
    let inspected = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap();
    let expires = millis_of(inspected.lease.unwrap().expires_at.as_ref().unwrap());
    assert!(
        expires >= resumed_at + grace_ms - 100,
        "every lease has lease.grace after the resume: {expires} < {resumed_at} + {grace_ms}"
    );
    daemon
        .client
        .heartbeat_workspace(&repo.id, &ws.id, &lease.id)
        .await
        .unwrap();
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn resume_token_is_checked() {
    let (daemon, _repo) = fixture().await;
    let quiesced = daemon.client.quiesce("", Duration::ZERO).await.unwrap();
    let error = daemon.client.resume("0000000000000000").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error:?}");
    let error = daemon.client.quiesce("", Duration::ZERO).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error:?}");
    let dest = daemon.tmp.path().join("backup");
    let error = daemon.client.backup(&dest, "").await;
    let refused = match error {
        Err(error) => error,
        Ok(mut stream) => stream.message().await.map_err(Into::into).unwrap_err(),
    };
    assert_eq!(
        refused.kind(),
        ErrorKind::Unavailable,
        "a backup waits its turn too"
    );

    let resumed = daemon.client.resume(&quiesced.token).await.unwrap();
    assert!(resumed.paused_ms < 60_000);
    let again = daemon.client.quiesce("", Duration::ZERO).await.unwrap();
    daemon.client.resume(&again.token).await.unwrap();
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn restore_to_new_root() {
    let (daemon, repo) = fixture().await;
    let dest = daemon.tmp.path().join("backup");
    backup(&daemon, &dest, "").await;
    let root = daemon.tmp.path().join("new-root");
    std::fs::create_dir(&root).unwrap();
    let into = daemon.tmp.path().join("new-state");
    let mut opts = options(&daemon.config, &into);
    opts.root = Some(root.clone());
    let report = run_restore(&dest, opts).await.unwrap();
    assert!(report.default_workspace_recreated);

    assert!(root.join(".jj/repo").is_dir());
    assert_eq!(
        std::fs::read_to_string(root.join(".jj/working_copy/type")).unwrap(),
        "local"
    );
    // The files of the default workspace's commit.
    let tracked = |dir: &Path| -> BTreeSet<PathBuf> {
        walkdir::WalkDir::new(dir)
            .into_iter()
            .filter_entry(|entry| entry.file_name() != ".jj")
            .map(Result::unwrap)
            .filter(|entry| !entry.file_type().is_dir())
            .map(|entry| entry.path().strip_prefix(dir).unwrap().to_owned())
            .collect()
    };
    // The original root also has ignored files (`target/`), never
    // committed.
    let files = tracked(&root);
    assert!(files.is_subset(&tracked(&repo.root)));
    let committed = open(&repo.root).await.tracked_file_count().unwrap();
    assert_eq!(u64::try_from(files.len()).unwrap(), committed);
    let db = reef_daemon::db::Db::open(&into.join("reef.sqlite3")).unwrap();
    let record = db.repository(repo.id.parse().unwrap()).unwrap();
    assert_eq!(record.root, std::fs::canonicalize(&root).unwrap());

    match std::process::Command::new("jj")
        .args(["status", "--no-pager"])
        .current_dir(&root)
        .env("JJ_CONFIG", "/dev/null")
        .env("JJ_USER", "Reef Test")
        .env("JJ_EMAIL", "reef-test@example.com")
        .output()
    {
        Ok(output) => assert!(
            output.status.success(),
            "jj status: {}",
            String::from_utf8_lossy(&output.stderr)
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("skipped: jj status: jj not on PATH");
        }
        Err(error) => panic!("run jj: {error}"),
    }
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn colocated_repo_round_trips() {
    logs();
    let daemon = TestDaemon::start_with(brief_config).await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    let root = daemon.tmp.path().join("colocated");
    std::fs::create_dir(&root).unwrap();
    std::fs::write(root.join("a.txt"), "a\n").unwrap();
    let at = root.clone();
    tokio::task::spawn_blocking(move || {
        let mut repo = pollster::block_on(Repo::init_colocated_git(&at, &test_settings())).unwrap();
        pollster::block_on(repo.snapshot("snapshot")).unwrap();
    })
    .await
    .unwrap();
    let registered = daemon
        .client
        .register_repository(&root, IfNotJj::Fail)
        .await
        .unwrap();
    let repo = registered.repository.unwrap().id;
    let root = std::fs::canonicalize(&root).unwrap();
    let ws = create(&daemon, &repo, None).await;
    write(&ws, "b.txt", "b\n");
    let (commit, _) = snapshot(&daemon, &repo, &ws).await;

    let dest = daemon.tmp.path().join("backup");
    let (_, manifest) = backup(&daemon, &dest, &repo).await;
    assert!(manifest.repos[0].colocated);
    let git = dest.join("repos").join(&repo).join("git");
    assert!(git.join("objects").is_dir(), ".git/objects was copied");
    assert!(git.join("HEAD").is_file());
    assert!(!git.join("index").exists(), "git's index is left out");
    let stopped = daemon.stop_keeping().await;
    let state_dir = stopped.config.paths.state_dir.clone();
    remove(&root.join(".jj/repo"));
    remove(&root.join(".git"));
    remove(&state_dir.join("repos"));

    let mut opts = options(&stopped.config, &state_dir);
    opts.force = true;
    run_restore(&dest, opts).await.unwrap();
    assert!(root.join(".git/objects").is_dir());
    let reopened = open(&root).await;
    assert!(reopened.is_colocated());
    drop(reopened);
    assert_eq!(
        file_at(&root, &commit, "b.txt").await.as_deref(),
        Some("b\n")
    );

    let daemon = stopped.start().await;
    let restored = workspaces(&daemon, &repo).await;
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].state, "active");
    assert_eq!(restored[0].commit_id, commit);
    assert_eq!(
        std::fs::read_to_string(Path::new(&restored[0].path).join("b.txt")).unwrap(),
        "b\n"
    );
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn gc_respects_retention() {
    for (retention, kept) in [(Duration::from_hours(1), true), (Duration::ZERO, false)] {
        let (daemon, repo) = fixture_with(|config| {
            config.file.gc.undo_window = Duration::ZERO;
            config.file.backup.retention = retention;
        })
        .await;
        daemon.handle.state().manager.scheduler().pause_watcher();
        let ws = create(&daemon, &repo.id, None).await;
        write(&ws, "a.txt", &"first\n".repeat(1000));
        let (hidden, _) = snapshot(&daemon, &repo.id, &ws).await;
        backup(&daemon, &daemon.tmp.path().join("backup"), &repo.id).await;
        write(&ws, "b.txt", &"second\n".repeat(1000));
        snapshot(&daemon, &repo.id, &ws).await;
        write(&ws, "c.txt", &"third\n".repeat(1000));
        snapshot(&daemon, &repo.id, &ws).await;
        past_this_second().await;

        let report = daemon
            .client
            .gc(&repo.id, false)
            .await
            .unwrap()
            .reports
            .remove(0);
        assert_eq!(report.jj_error, "", "{report:?}");
        assert_eq!(
            has_commit(&repo.root, &hidden).await,
            kept,
            "retention {retention:?}: the commit hidden after the backup"
        );
    }
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn rematerialize_on_demand() {
    let (daemon, repo) = fixture().await;
    let a = create(&daemon, &repo.id, None).await;
    write(&a, "a.txt", "a\n");
    let (a_commit, _) = snapshot(&daemon, &repo.id, &a).await;
    // On a's change, so a second tree.
    let b = create(
        &daemon,
        &repo.id,
        Some(create_request::Start::NewOn(a.change_id.clone())),
    )
    .await;
    // On the default start again: a's first tree, shared.
    let c = create(&daemon, &repo.id, None).await;
    let dest = daemon.tmp.path().join("backup");
    backup(&daemon, &dest, &repo.id).await;
    let stopped = daemon.stop_keeping().await;
    let state_dir = stopped.config.paths.state_dir.clone();
    remove(&state_dir.join("repos"));
    let mut opts = options(&stopped.config, &state_dir);
    opts.force = true;
    run_restore(&dest, opts).await.unwrap();
    for ws in [&a, &b, &c] {
        assert!(
            !Path::new(&ws.path).exists(),
            "no directory before the start"
        );
    }

    let daemon = stopped.start().await;
    let state = daemon.handle.state();
    assert_eq!(state.db.meta(META_REMATERIALIZE_PENDING).unwrap(), None);
    let restored = workspaces(&daemon, &repo.id).await;
    assert_eq!(restored.len(), 3);
    for ws in &restored {
        assert_eq!(ws.state, "active", "none is reclaimed: {ws:?}");
        assert!(Path::new(&ws.path).is_dir());
        assert!(
            logs().has_line(&[
                "workspace rematerialized after a restore",
                &format!("workspace_id={}", ws.id),
            ]),
            "the start-up pass logs each one"
        );
    }
    let a_restored = restored.iter().find(|ws| ws.id == a.id).unwrap();
    assert_eq!(a_restored.commit_id, a_commit);
    assert_eq!(
        std::fs::read_to_string(Path::new(&a_restored.path).join("a.txt")).unwrap(),
        "a\n"
    );
    let trees: BTreeSet<String> = state
        .db
        .workspaces(None, &[])
        .unwrap()
        .into_iter()
        .map(|row| row.base_tree_id)
        .collect();
    let bases = state.db.bases(repo.id.parse().unwrap()).unwrap();
    assert_eq!(bases.len(), trees.len(), "one base per distinct tree");
    assert_eq!(trees.len(), 2, "{trees:?}");
}

// The pause of step 3 on the medium preset, against the brief's 100 ms.
// Run with `cargo test --release -p reef-daemon --test backup -- --ignored
// --nocapture`.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "manual measurement on the medium preset; prints its numbers"]
#[allow(
    clippy::print_stderr,
    reason = "a manual measurement prints its numbers"
)]
async fn medium_pause_is_short() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Medium, brief_config).await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    let ws = create(&daemon, &repo.id, None).await;
    for round in 0..200 {
        write(&ws, "round.txt", &format!("{round}\n"));
        snapshot(&daemon, &repo.id, &ws).await;
    }
    let began = SystemTime::now();
    let dest = daemon.tmp.path().join("backup");
    let (_, manifest) = backup(&daemon, &dest, &repo.id).await;
    let entry = &manifest.repos[0];
    eprintln!(
        "medium: pause {} ms, {} files, {} bytes, backup {} ms, {} operations",
        entry.pause_ms,
        entry.files,
        entry.bytes,
        began.elapsed().unwrap().as_millis(),
        op_log(&repo.root).await.len()
    );
    assert!(entry.pause_ms < 100, "pause {} ms", entry.pause_ms);
}

/// The daemon's log lines in this test process, from the one subscriber
/// every test here installs (the first call wins; nextest runs each test in
/// its own process, `cargo test` shares one). `TEST_LOG=reef_daemon=debug`
/// also prints them.
fn logs() -> &'static LogCapture {
    static LOGS: std::sync::OnceLock<LogCapture> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let echo = std::env::var("TEST_LOG").ok();
        let capture = LogCapture(Arc::default(), echo.is_some());
        let writer = capture.clone();
        let filter = echo.unwrap_or_else(|| "reef_daemon=info".to_owned());
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .try_init();
        capture
    })
}

/// Captured log text, and whether to echo it; see [`logs`].
#[derive(Clone)]
struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>, bool);

impl LogCapture {
    /// Whether one line holds every one of `parts`.
    fn has_line(&self, parts: &[&str]) -> bool {
        String::from_utf8_lossy(&self.0.lock().unwrap())
            .lines()
            .any(|line| parts.iter().all(|part| line.contains(part)))
    }
}

impl std::io::Write for LogCapture {
    #[allow(clippy::print_stderr, reason = "TEST_LOG echoes the daemon's logs")]
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        if self.1 {
            eprint!("{}", String::from_utf8_lossy(bytes));
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
