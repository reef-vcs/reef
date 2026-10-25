//! Shared dependency stores end to end (R1.20): building and sealing,
//! mounting into workspaces read-only, the missing-store rule, lockfile
//! mismatch detection, failed and timed-out builds, and unmounting before
//! tear-down. Numbered as the brief numbers them; R1.20 adds 13 (restart)
//! and 14 (GC).
//!
//! The fixture is a `small` synthetic repository whose root commit gains a
//! `reef.toml` declaring store `deps` (lockfile `deps.lock`, mounted at
//! `vendor/deps`). On a daemon with `CAP_SYS_ADMIN` (CI's root step) the
//! mounts are read-only bind mounts; elsewhere symlinks.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::{SyntheticRepo, TestDaemon, logs, test_settings};
use reef_api::client::ClientError;
use reef_api::proto::v1::{
    self, CreateRequest, ListWorkspacesRequest, Workspace, build_event, create_request,
    mounted_store, output_line,
};
use reef_daemon::store::hash_lockfile;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::{CommitIdHex, Repo, StartPoint};
use reef_testing::synth::Preset;

const OWNER: &str = "tester@host";
const LOCKFILE: &str = "v1\n";

/// One store whose build copies the lockfile into the store and says so.
const SPEC: &str = r#"
[[store]]
name = "deps"
lockfile = "deps.lock"
mount = "vendor/deps"
build = ["sh", "-c", "mkdir -p \"$REEF_STORE_DIR\" && cp deps.lock \"$REEF_STORE_DIR/manifest\" && echo built"]
"#;

/// `SPEC` with another build command.
fn spec_building(command: &str) -> String {
    format!(
        "[[store]]\nname = \"deps\"\nlockfile = \"deps.lock\"\nmount = \"vendor/deps\"\nbuild = [\"sh\", \"-c\", {command:?}]\n"
    )
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
}

async fn fixture_with(
    configure: impl FnOnce(&mut reef_daemon::config::Config),
) -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, configure).await;
    commit_at_root(&repo.root, &[("reef.toml", SPEC), ("deps.lock", LOCKFILE)]).await;
    (daemon, repo)
}

/// Writes `files` at the registered root and snapshots its `default`
/// workspace, which a create or build without a revision starts on.
async fn commit_at_root(root: &Path, files: &[(&str, &str)]) {
    for (path, contents) in files {
        std::fs::write(root.join(path), contents).unwrap();
    }
    let root = root.to_owned();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let mut repo = Repo::open(&root, &test_settings()).await.unwrap();
            repo.snapshot("edit reef.toml").await.unwrap();
        });
    })
    .await
    .unwrap();
}

fn key_hash() -> String {
    hash_lockfile(LOCKFILE.as_bytes())
}

async fn create(
    daemon: &TestDaemon,
    repo: &SyntheticRepo,
    missing: create_request::MissingStores,
) -> Result<Workspace, ClientError> {
    let response = daemon
        .client
        .create_workspace(CreateRequest {
            repo: repo.id.clone(),
            owner: OWNER.to_owned(),
            missing_stores: missing.into(),
            ..CreateRequest::default()
        })
        .await?;
    Ok(response.workspace.unwrap())
}

async fn create_ok(daemon: &TestDaemon, repo: &SyntheticRepo) -> Workspace {
    create(daemon, repo, create_request::MissingStores::Fail)
        .await
        .unwrap()
}

/// What a `Stores.Build` stream carried.
#[derive(Debug, Default)]
struct Built {
    /// Each `Started` event, with how many lines came before it.
    started: Vec<(usize, v1::BuildStarted)>,
    lines: Vec<v1::OutputLine>,
    done: Option<v1::BuildResult>,
    error: Option<ClientError>,
}

async fn build_with(daemon: &TestDaemon, repo: &SyntheticRepo, timeout: Duration) -> Built {
    let mut built = Built::default();
    let mut stream = match daemon
        .client
        .build_store(&repo.id, "deps", "", timeout, false)
        .await
    {
        Ok(stream) => stream,
        Err(error) => {
            built.error = Some(error);
            return built;
        }
    };
    loop {
        match stream.message().await {
            Ok(Some(event)) => match event.event.unwrap() {
                build_event::Event::Started(started) => {
                    built.started.push((built.lines.len(), started));
                }
                build_event::Event::Line(line) => built.lines.push(line),
                build_event::Event::Done(done) => built.done = Some(done),
            },
            Ok(None) => return built,
            Err(status) => {
                built.error = Some(ClientError::from(status));
                return built;
            }
        }
    }
}

/// The first event after the build's `Started` (RX.01), which comes first.
async fn first_line(events: &mut tonic::Streaming<v1::BuildEvent>) -> v1::BuildEvent {
    let started = events.message().await.unwrap().unwrap();
    assert!(
        matches!(started.event, Some(build_event::Event::Started(_))),
        "{started:?}"
    );
    events.message().await.unwrap().unwrap()
}

async fn build(daemon: &TestDaemon, repo: &SyntheticRepo) -> v1::BuildResult {
    let built = build_with(daemon, repo, Duration::from_mins(5)).await;
    assert!(built.error.is_none(), "{:?}", built.error);
    built.done.expect("a result")
}

fn mount_of(ws: &Workspace) -> PathBuf {
    Path::new(&ws.path).join("vendor/deps")
}

/// Whether this daemon gives workspaces bind mounts: privileged, and not
/// a FUSE overlay workspace.
fn expects_bind(daemon: &TestDaemon, ws: &Workspace) -> bool {
    daemon.handle.state().manager.stores().privileged() && ws.backend != "fuse-overlayfs"
}

/// Where the store's contents are: `<hash>/<mount's last component>/`.
fn store_dir(daemon: &TestDaemon) -> PathBuf {
    daemon
        .state_dir()
        .join("stores")
        .join("deps")
        .join(key_hash())
        .join("deps")
}

async fn in_use(daemon: &TestDaemon) -> u32 {
    let stores = daemon.client.list_stores("").await.unwrap();
    stores
        .iter()
        .find(|store| store.name == "deps" && store.lockfile_hash == key_hash())
        .map_or(0, |store| store.in_use_by)
}

/// Asserts `ws` has the store at its mount path the way this daemon mounts
/// it: a symlink to the store directory, or a bind mount of it.
fn assert_mounted(daemon: &TestDaemon, ws: &Workspace) {
    let mount = mount_of(ws);
    assert_eq!(ws.stores.len(), 1, "{:?}", ws.stores);
    let mounted = &ws.stores[0];
    assert_eq!(mounted.name, "deps");
    assert_eq!(mounted.lockfile_hash, key_hash());
    assert_eq!(mounted.mount, "vendor/deps");
    assert!(mounted.verified);
    let meta = std::fs::symlink_metadata(&mount).unwrap();
    if expects_bind(daemon, ws) {
        assert_eq!(mounted.mode(), mounted_store::Mode::Bind);
        assert!(meta.is_dir());
        let store = std::fs::metadata(store_dir(daemon)).unwrap();
        assert_eq!((meta.dev(), meta.ino()), (store.dev(), store.ino()));
    } else {
        assert_eq!(mounted.mode(), mounted_store::Mode::Symlink);
        assert!(meta.file_type().is_symlink());
        assert_eq!(std::fs::read_link(&mount).unwrap(), store_dir(daemon));
    }
    assert_eq!(
        std::fs::read_to_string(mount.join("manifest")).unwrap(),
        LOCKFILE
    );
}

/// Every file and directory under `dir` without a write bit.
fn assert_sealed(dir: &Path) {
    for entry in walkdir(dir) {
        let mode = std::fs::symlink_metadata(&entry)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o222,
            0,
            "{} is writable ({mode:o})",
            entry.display()
        );
    }
}

fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut found = vec![dir.to_owned()];
    let mut index = 0;
    while index < found.len() {
        let path = found[index].clone();
        if path.is_dir() {
            for entry in std::fs::read_dir(&path).unwrap() {
                found.push(entry.unwrap().path());
            }
        }
        index += 1;
    }
    found
}

fn libc_erofs() -> i32 {
    nix::errno::Errno::EROFS as i32
}

fn is_root() -> bool {
    nix::unistd::geteuid().is_root()
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn create_without_store_fails_unavailable() {
    let (daemon, repo) = fixture().await;
    let error = create(&daemon, &repo, create_request::MissingStores::Fail)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let message = error.to_string();
    assert!(message.contains(&key_hash()), "{message}");
    // The fix is the detail's `next` (R1.26), not part of the message.
    let advice = error.advice().unwrap();
    assert_eq!(
        advice.next[..6],
        ["reef", "store", "build", "deps", "--repo", repo.id.as_str()]
    );
    let listed = daemon
        .client
        .list_workspaces(ListWorkspacesRequest {
            repo: repo.id.clone(),
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap();
    assert!(listed.is_empty(), "a failed create leaves no row");
}

// 2
#[tokio::test(flavor = "multi_thread")]
async fn create_with_skip_marks_attention() {
    let (daemon, repo) = fixture().await;
    let ws = create(&daemon, &repo, create_request::MissingStores::Skip)
        .await
        .unwrap();
    assert_eq!(ws.attention, "store-missing");
    assert!(ws.stores.is_empty(), "nothing mounted: {:?}", ws.stores);
    assert!(std::fs::symlink_metadata(mount_of(&ws)).is_err());
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn build_seals_and_lists() {
    let (daemon, repo) = fixture().await;
    let built = build_with(&daemon, &repo, Duration::from_mins(5)).await;
    assert!(built.error.is_none(), "{:?}", built.error);
    assert!(
        built
            .lines
            .iter()
            .any(|line| line.text == "built" && line.stream() == output_line::Stream::Stdout),
        "{:?}",
        built.lines
    );
    let done = built.done.unwrap();
    assert!(done.sealed);
    assert_eq!(done.exit_code, 0);
    assert_eq!(done.name, "deps");
    assert_eq!(done.lockfile_hash, key_hash());
    assert!(!done.op_id.is_empty(), "the scratch workspace's release");
    assert!(!done.already_built);
    let dir = store_dir(&daemon);
    assert_eq!(Path::new(&done.path), dir);
    assert_eq!(
        std::fs::read_to_string(dir.join("manifest")).unwrap(),
        LOCKFILE
    );
    assert_sealed(&dir);

    let stores = daemon.client.list_stores(&repo.id).await.unwrap();
    assert_eq!(stores.len(), 1, "{stores:?}");
    assert!(stores[0].sealed);
    assert_eq!(stores[0].in_use_by, 0);
    assert_eq!(stores[0].repo_id, repo.id);
    assert!(stores[0].built_at.is_some());

    // The scratch workspace is gone, released as a store build.
    let all = daemon
        .client
        .list_workspaces(ListWorkspacesRequest {
            repo: repo.id.clone(),
            ..ListWorkspacesRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(all.len(), 1, "{all:?}");
    assert_eq!(all[0].owner, "reef-store-build");
    assert_eq!(all[0].state, "released");
    assert!(!Path::new(&all[0].path).exists());

    // Built already: answered without a build.
    let again = build(&daemon, &repo).await;
    assert!(again.sealed && again.already_built);
    assert!(again.op_id.is_empty(), "no build ran: {}", again.op_id);
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn create_mounts_sealed_store() {
    let (daemon, repo) = fixture().await;
    build(&daemon, &repo).await;
    let ws = create_ok(&daemon, &repo).await;
    assert_eq!(ws.attention, "");
    assert_mounted(&daemon, &ws);
    assert_eq!(in_use(&daemon).await, 1);
    // Inspect and List carry it too.
    let inspected = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap();
    assert_eq!(inspected.workspace.unwrap().stores, ws.stores);

    // A forced rebuild would move the store out from under the mount.
    let mut forced = daemon
        .client
        .build_store(&repo.id, "deps", "", Duration::from_mins(1), true)
        .await
        .unwrap();
    let error = ClientError::from(forced.message().await.unwrap_err());
    assert_eq!(error.kind(), ErrorKind::Conflict, "{error}");
    assert!(error.to_string().contains("mounted by 1"), "{error}");
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn store_is_excluded_from_snapshots() {
    let (daemon, repo) = fixture().await;
    build(&daemon, &repo).await;
    let ws = create_ok(&daemon, &repo).await;
    let mount = mount_of(&ws);
    let error = std::fs::write(mount.join("poison"), "x").unwrap_err();
    if expects_bind(&daemon, &ws) {
        assert_eq!(error.raw_os_error(), Some(libc_erofs()), "{error}");
    } else if !is_root() {
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::PermissionDenied,
            "{error}"
        );
    }
    std::fs::write(Path::new(&ws.path).join("vendor/next.txt"), "beside it\n").unwrap();
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    assert!(snapshot.changed);

    let root = repo.root.clone();
    let commit = snapshot.commit_id.parse().unwrap();
    let (next, deps, manifest) = tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let repo = Repo::open(&root, &test_settings()).await.unwrap();
            let mut ids = Vec::new();
            for path in ["vendor/next.txt", "vendor/deps", "vendor/deps/manifest"] {
                let path = reef_lib::jj::RepoPath::from_internal_string(path).unwrap();
                ids.push(repo.file_id_at(&commit, path).await.unwrap());
            }
            (ids[0].clone(), ids[1].clone(), ids[2].clone())
        })
    })
    .await
    .unwrap();
    assert!(next.is_some(), "the file beside the mount is recorded");
    assert_eq!(deps, None, "the mount itself is not in the tree");
    assert_eq!(manifest, None, "nothing under it is");
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn two_workspaces_share_one_store() {
    let (daemon, repo) = fixture().await;
    build(&daemon, &repo).await;
    let one = create_ok(&daemon, &repo).await;
    let two = create_ok(&daemon, &repo).await;
    assert_mounted(&daemon, &one);
    assert_mounted(&daemon, &two);
    let inode = |ws: &Workspace| {
        std::fs::metadata(mount_of(ws).join("manifest"))
            .unwrap()
            .ino()
    };
    assert_eq!(inode(&one), inode(&two), "one directory");
    assert_eq!(in_use(&daemon).await, 2);
    daemon
        .client
        .release_workspace(&repo.id, &one.id, true, false)
        .await
        .unwrap();
    assert_eq!(in_use(&daemon).await, 1);
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn lockfile_edit_sets_mismatch() {
    let (daemon, repo) = fixture().await;
    build(&daemon, &repo).await;
    let ws = create_ok(&daemon, &repo).await;
    let lockfile = Path::new(&ws.path).join("deps.lock");
    std::fs::write(&lockfile, "v1\nv2\n").unwrap();
    daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let inspected = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(inspected.attention, "store-hash-mismatch");
    assert!(!inspected.stores[0].verified);
    assert!(mount_of(&ws).join("manifest").is_file(), "the mount stays");

    // Back to the hashed contents: the mark goes.
    std::fs::write(&lockfile, LOCKFILE).unwrap();
    daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let inspected = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(inspected.attention, "");
    assert!(inspected.stores[0].verified);
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn build_failure_keeps_failed_dir() {
    let (daemon, repo) = fixture().await;
    let failing = spec_building("echo partial > \"$REEF_STORE_DIR/partial\"; exit 3");
    commit_at_root(&repo.root, &[("reef.toml", &failing)]).await;
    let done = build(&daemon, &repo).await;
    assert_eq!(done.exit_code, 3);
    assert!(!done.sealed);
    assert!(done.path.contains(".failed-"), "{}", done.path);
    assert_eq!(
        std::fs::read_to_string(Path::new(&done.path).join("deps/partial")).unwrap(),
        "partial\n"
    );
    assert!(!store_dir(&daemon).exists());
    assert!(
        daemon.client.list_stores("").await.unwrap().is_empty(),
        "nothing sealed"
    );
    let error = create(&daemon, &repo, create_request::MissingStores::Fail)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable);
    // A later build of the same store is not blocked by the failed one.
    commit_at_root(&repo.root, &[("reef.toml", SPEC)]).await;
    assert!(build(&daemon, &repo).await.sealed);
}

/// The processes whose command line is `command`, as `ps` lists them;
/// empty if none. Exact, so a shell whose script mentions it does not
/// count.
fn running(command: &str) -> String {
    let output = std::process::Command::new("ps")
        .args(["-axo", "pid=,pgid=,command="])
        .output()
        .unwrap();
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| {
            let mut fields = line.split_whitespace();
            let (_, _) = (fields.next(), fields.next());
            fields.collect::<Vec<_>>().join(" ") == command
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn build_timeout_kills_process_group() {
    let (daemon, repo) = fixture().await;
    // The `sleep` runs in the background, so killing `sh` alone would leave
    // it; only killing the group ends it.
    let slow = spec_building("echo started; sleep 3171 & wait");
    commit_at_root(&repo.root, &[("reef.toml", &slow)]).await;
    let mut events = daemon
        .client
        .build_store(&repo.id, "deps", "", Duration::from_secs(2), false)
        .await
        .unwrap();
    // Timed from the command's start, not the scratch workspace's create.
    let started = first_line(&mut events).await;
    assert!(
        matches!(started.event, Some(build_event::Event::Line(ref line)) if line.text == "started"),
        "{started:?}"
    );
    let begun = Instant::now();
    let error = loop {
        match events.message().await {
            Ok(Some(event)) => {
                assert!(
                    !matches!(event.event, Some(build_event::Event::Done(_))),
                    "{event:?}"
                );
            }
            Ok(None) => panic!("the stream ended without an error"),
            Err(status) => break ClientError::from(status),
        }
    };
    assert!(
        begun.elapsed() < Duration::from_secs(10),
        "{:?}",
        begun.elapsed()
    );
    assert_eq!(error.kind(), ErrorKind::Internal, "{error}");
    assert!(error.to_string().contains("timed out"), "{error}");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !running("sleep 3171").is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let left = running("sleep 3171");
    assert!(
        left.is_empty(),
        "the build's process group is still running:\n{left}"
    );
    assert!(
        daemon.client.list_stores("").await.unwrap().is_empty(),
        "nothing sealed"
    );
}

// 9b: a build that exits leaves nothing of its process group behind, so
// nothing writes into the store once it is sealed.
#[tokio::test(flavor = "multi_thread")]
async fn build_leaves_no_processes() {
    let (daemon, repo) = fixture().await;
    let leaky = spec_building("sleep 3173 > /dev/null 2>&1 & mkdir -p \"$REEF_STORE_DIR\"");
    commit_at_root(&repo.root, &[("reef.toml", &leaky)]).await;
    assert!(build(&daemon, &repo).await.sealed);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !running("sleep 3173").is_empty() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let left = running("sleep 3173");
    assert!(left.is_empty(), "the build left processes running:\n{left}");
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_build_rejected() {
    let (daemon, repo) = fixture().await;
    let slow = spec_building("echo started; sleep 3; mkdir -p \"$REEF_STORE_DIR\"");
    commit_at_root(&repo.root, &[("reef.toml", &slow)]).await;
    let mut first = daemon
        .client
        .build_store(&repo.id, "deps", "", Duration::from_mins(1), false)
        .await
        .unwrap();
    let started = first_line(&mut first).await;
    assert!(
        matches!(started.event, Some(build_event::Event::Line(ref line)) if line.text == "started"),
        "{started:?}"
    );
    let second = build_with(&daemon, &repo, Duration::from_mins(1)).await;
    let error = second.error.expect("refused");
    assert_eq!(error.kind(), ErrorKind::AlreadyExists, "{error}");
    let mut done = None;
    while let Some(event) = first.message().await.unwrap() {
        if let Some(build_event::Event::Done(result)) = event.event {
            done = Some(result);
        }
    }
    assert!(done.unwrap().sealed, "the first build finished");
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn release_unmounts_before_teardown() {
    let (daemon, repo) = fixture().await;
    build(&daemon, &repo).await;
    let ws = create_ok(&daemon, &repo).await;
    daemon
        .client
        .release_workspace(&repo.id, &ws.id, true, false)
        .await
        .unwrap();
    assert!(!Path::new(&ws.path).exists());
    let dir = store_dir(&daemon);
    assert_eq!(
        std::fs::read_to_string(dir.join("manifest")).unwrap(),
        LOCKFILE
    );
    assert_sealed(&dir);
    assert_eq!(in_use(&daemon).await, 0);
}

// 12
#[cfg(all(target_os = "linux", feature = "privileged-tests"))]
#[tokio::test(flavor = "multi_thread")]
async fn bind_mount_is_read_only() {
    let (daemon, repo) = fixture().await;
    if !daemon.handle.state().manager.stores().privileged() {
        eprintln!("skipped: needs CAP_SYS_ADMIN (run as root)");
        return;
    }
    build(&daemon, &repo).await;
    let ws = create_ok(&daemon, &repo).await;
    if ws.backend == "fuse-overlayfs" {
        eprintln!("skipped: FUSE overlay workspaces get symlinks");
        return;
    }
    assert_eq!(ws.stores[0].mode(), mounted_store::Mode::Bind);
    let mount = mount_of(&ws);
    let error = std::fs::write(mount.join("poison"), "x").unwrap_err();
    assert_eq!(error.raw_os_error(), Some(libc_erofs()), "{error}");
    // Root ignores mode bits; only the read-only mount stops it, and the
    // workspace side cannot make that writable.
    let chmod = std::fs::set_permissions(&mount, std::fs::Permissions::from_mode(0o777));
    assert_eq!(chmod.unwrap_err().raw_os_error(), Some(libc_erofs()));
    assert!(std::fs::write(mount.join("poison"), "x").is_err());
    assert_eq!(
        std::fs::metadata(store_dir(&daemon))
            .unwrap()
            .permissions()
            .mode()
            & 0o222,
        0
    );
}

// 13: a restart finds each workspace's mount again.
#[tokio::test(flavor = "multi_thread")]
async fn restart_finds_mounts_again() {
    let (daemon, repo) = fixture().await;
    build(&daemon, &repo).await;
    let snapshotted = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&snapshotted);
    daemon
        .handle
        .state()
        .manager
        .scheduler()
        .on_snapshot(Box::new(move |outcome| {
            sink.lock().unwrap().push(outcome.workspace);
        }));
    let ws = create_ok(&daemon, &repo).await;
    if cfg!(target_os = "linux") {
        // The commit below rebases the workspace onto a new lockfile, which
        // a snapshot of it then reports as `store-hash-mismatch`. On Linux
        // its watch attaches in the background and a full scan follows
        // (R1.32): let that one land first, as nothing snapshots it after.
        let id: reef_daemon::model::WorkspaceId = ws.id.parse().unwrap();
        let begun = Instant::now();
        while !snapshotted.lock().unwrap().contains(&id) {
            assert!(
                begun.elapsed() < Duration::from_secs(30),
                "the attach's snapshot"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    let skipped = {
        // A workspace created before the store was rebuilt for a new
        // lockfile, without it.
        commit_at_root(&repo.root, &[("deps.lock", "v2\n")]).await;
        create(&daemon, &repo, create_request::MissingStores::Skip)
            .await
            .unwrap()
    };
    let daemon = daemon.restart().await;
    let reloaded = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(reloaded.stores, ws.stores);
    assert_mounted(&daemon, &reloaded);
    assert_eq!(in_use(&daemon).await, 1);
    let skipped = daemon
        .client
        .inspect_workspace(&repo.id, &skipped.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert!(
        skipped.stores.is_empty(),
        "not mounted: {:?}",
        skipped.stores
    );
    assert_eq!(skipped.attention, "store-missing");
}

// 14: GC removes a store nothing names or mounts, and keeps the others.
#[tokio::test(flavor = "multi_thread")]
async fn gc_removes_unused_stores() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.undo_window = Duration::ZERO;
    })
    .await;
    build(&daemon, &repo).await;
    let old = store_dir(&daemon);
    // The default start's lockfile moves on; v1's store is unused.
    commit_at_root(&repo.root, &[("deps.lock", "v2\n")]).await;
    let v2 = build(&daemon, &repo).await;
    assert!(v2.sealed);
    // A failed build's directory is a leftover GC removes too.
    commit_at_root(
        &repo.root,
        &[
            ("reef.toml", &spec_building("exit 1")),
            ("deps.lock", "v3\n"),
        ],
    )
    .await;
    let failed = build(&daemon, &repo).await;
    assert!(!failed.sealed);
    commit_at_root(&repo.root, &[("reef.toml", SPEC), ("deps.lock", "v2\n")]).await;
    let mounted = create_ok(&daemon, &repo).await;
    assert_eq!(mounted.stores[0].lockfile_hash, v2.lockfile_hash);
    tokio::time::sleep(Duration::from_millis(10)).await;

    let dry = daemon.client.gc(&repo.id, true).await.unwrap();
    assert_eq!(dry.reports[0].stores_removed, 2, "{:?}", dry.reports[0]);
    assert!(old.exists(), "a dry run removes nothing");
    let report = daemon.client.gc(&repo.id, false).await.unwrap();
    let report = &report.reports[0];
    assert_eq!(report.stores_removed, 2, "{report:?}");
    assert!(report.store_bytes_freed > 0);
    assert!(!old.exists());
    assert!(!Path::new(&failed.path).exists());
    assert!(
        Path::new(&v2.path).join("manifest").is_file(),
        "in use and named"
    );
    let listed = daemon.client.list_stores("").await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].lockfile_hash, v2.lockfile_hash);
}

/// R1.06: after a lockfile edit, a move to the workspace's own change
/// fails while the new lockfile has no build, changing nothing, then
/// remounts the store once it is built (ADR 0022's fix for a mismatch).
#[tokio::test(flavor = "multi_thread")]
async fn move_to_own_change_remounts_rebuilt_store() {
    let (daemon, repo) = fixture().await;
    build(&daemon, &repo).await;
    let ws = create_ok(&daemon, &repo).await;
    std::fs::write(Path::new(&ws.path).join("deps.lock"), "v2\n").unwrap();
    let snapshot = daemon
        .client
        .snapshot_workspace(&repo.id, &ws.id, "")
        .await
        .unwrap();
    let to_own_change = |change: String| v1::MoveRequest {
        repo: repo.id.clone(),
        workspace: ws.id.clone(),
        target: Some(v1::move_request::Target::Change(change)),
        ..v1::MoveRequest::default()
    };

    let error = daemon
        .client
        .move_workspace(to_own_change(ws.change_id.clone()))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error:?}");
    let row = daemon
        .client
        .inspect_workspace(&repo.id, &ws.id, false)
        .await
        .unwrap()
        .workspace
        .unwrap();
    assert_eq!(row.commit_id, snapshot.commit_id, "nothing moved");
    assert_eq!(row.attention, "store-hash-mismatch");
    assert_eq!(
        row.stores[0].lockfile_hash,
        key_hash(),
        "the old store stays"
    );
    assert!(mount_of(&ws).join("manifest").is_file());

    let mut stream = daemon
        .client
        .build_store(
            &repo.id,
            "deps",
            &snapshot.commit_id,
            Duration::from_mins(5),
            false,
        )
        .await
        .unwrap();
    while stream.message().await.unwrap().is_some() {}
    let moved = daemon
        .client
        .move_workspace(to_own_change(ws.change_id.clone()))
        .await
        .unwrap();
    assert_eq!(moved.op_id, "", "the pointer did not move");
    let after = moved.workspace.unwrap();
    assert_eq!(after.commit_id, snapshot.commit_id);
    assert_eq!(after.attention, "");
    assert_eq!(after.stores.len(), 1);
    assert_eq!(after.stores[0].lockfile_hash, hash_lockfile(b"v2\n"));
    assert!(after.stores[0].verified);
    assert_eq!(
        std::fs::read_to_string(mount_of(&ws).join("manifest")).unwrap(),
        "v2\n"
    );
    assert_eq!(in_use(&daemon).await, 0, "the old store is free");
}

// A package manager that deletes its output directory first (`npm ci`
// removes `node_modules`) replaces the build's link into the store with a
// directory of its own; that directory is what gets sealed.
#[tokio::test(flavor = "multi_thread")]
async fn build_that_replaces_the_link_is_sealed() {
    let (daemon, repo) = fixture().await;
    let replacing = spec_building(
        "rm vendor/deps && mkdir vendor/deps && echo installed > vendor/deps/pkg && echo built",
    );
    commit_at_root(&repo.root, &[("reef.toml", &replacing)]).await;
    let done = build(&daemon, &repo).await;
    assert!(done.sealed, "{done:?}");
    let dir = store_dir(&daemon);
    assert_eq!(Path::new(&done.path), dir);
    assert_eq!(
        std::fs::read_to_string(dir.join("pkg")).unwrap(),
        "installed\n"
    );
    assert_sealed(&dir);

    let ws = create_ok(&daemon, &repo).await;
    assert_eq!(
        std::fs::read_to_string(mount_of(&ws).join("pkg")).unwrap(),
        "installed\n"
    );
    // The mount's real path ends in the mount's own name, as Node needs to
    // find a package's dependencies in the `node_modules` above it.
    let real = std::fs::canonicalize(mount_of(&ws)).unwrap();
    assert_eq!(real.file_name().unwrap(), "deps", "{}", real.display());
}

// A build that exits 0 but leaves nothing it can seal fails, and nothing is
// sealed: removing the link, or replacing it after writing into the store.
#[tokio::test(flavor = "multi_thread")]
async fn build_that_breaks_the_link_fails() {
    let (daemon, repo) = fixture().await;
    for command in [
        "rm vendor/deps",
        "rm vendor/deps && ln -s /tmp vendor/deps",
        "rm vendor/deps && touch vendor/deps",
        "rm -rf vendor && touch vendor",
        "echo half > \"$REEF_STORE_DIR/half\" && rm vendor/deps && mkdir vendor/deps",
    ] {
        commit_at_root(&repo.root, &[("reef.toml", &spec_building(command))]).await;
        let built = build_with(&daemon, &repo, Duration::from_mins(5)).await;
        let error = built.error.expect(command);
        assert_eq!(error.kind(), ErrorKind::Internal, "{command}: {error}");
        assert!(
            error
                .to_string()
                .contains("build replaced its link at vendor/deps"),
            "{command}: {error}"
        );
        assert!(!store_dir(&daemon).exists(), "{command}");
        assert!(
            daemon.client.list_stores("").await.unwrap().is_empty(),
            "{command}: nothing sealed"
        );
    }
}

// A store sealed before its contents moved one level down: workspaces that
// mount it keep it, a create reports it missing, and its build waits for
// them to be released, then seals it in the new layout.
#[tokio::test(flavor = "multi_thread")]
async fn store_in_the_old_layout_is_rebuilt() {
    let (daemon, repo) = fixture().await;
    build(&daemon, &repo).await;
    let mounted = create_ok(&daemon, &repo).await;
    // The row as the old layout wrote it: the store directory itself.
    let root = store_dir(&daemon).parent().unwrap().to_owned();
    reef_daemon::db::Db::open(&daemon.state_dir().join("reef.sqlite3"))
        .unwrap()
        .seal_store("deps", &key_hash(), reef_daemon::model::Timestamp(1), &root)
        .unwrap();

    let error = create(&daemon, &repo, create_request::MissingStores::Fail)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let refused = build_with(&daemon, &repo, Duration::from_mins(5)).await;
    let error = refused.error.expect("refused while mounted");
    assert_eq!(error.kind(), ErrorKind::Conflict, "{error}");
    assert_eq!(
        std::fs::read_to_string(mount_of(&mounted).join("manifest")).unwrap(),
        LOCKFILE,
        "the mounted workspace keeps it"
    );

    daemon
        .client
        .release_workspace(&repo.id, &mounted.id, true, false)
        .await
        .unwrap();
    let done = build(&daemon, &repo).await;
    assert!(done.sealed && !done.already_built, "{done:?}");
    assert_eq!(Path::new(&done.path), store_dir(&daemon));
    let ws = create_ok(&daemon, &repo).await;
    assert_eq!(
        std::fs::read_to_string(mount_of(&ws).join("manifest")).unwrap(),
        LOCKFILE
    );
    // A symlink leads into the new layout; a bind mount's real path is the
    // workspace's own.
    if !expects_bind(&daemon, &ws) {
        let real = std::fs::canonicalize(mount_of(&ws)).unwrap();
        assert_eq!(real, std::fs::canonicalize(store_dir(&daemon)).unwrap());
    }
}

// A store is offered only to mounts with the name it was built for: one
// built at `vendor/deps` is missing for a spec that mounts it at `deps`
// under another directory name, and rebuilding it renames its contents.
#[tokio::test(flavor = "multi_thread")]
async fn store_built_for_another_mount_name_is_rebuilt() {
    let (daemon, repo) = fixture().await;
    build(&daemon, &repo).await;
    let elsewhere = SPEC.replace("mount = \"vendor/deps\"", "mount = \"lib/node_modules\"");
    commit_at_root(&repo.root, &[("reef.toml", &elsewhere)]).await;
    let error = create(&daemon, &repo, create_request::MissingStores::Fail)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    let done = build(&daemon, &repo).await;
    assert!(done.sealed && !done.already_built, "{done:?}");
    let contents = store_dir(&daemon).parent().unwrap().join("node_modules");
    assert_eq!(Path::new(&done.path), contents);
    let ws = create_ok(&daemon, &repo).await;
    let mount = Path::new(&ws.path).join("lib/node_modules");
    assert_eq!(
        std::fs::read_to_string(mount.join("manifest")).unwrap(),
        LOCKFILE
    );
    assert_eq!(
        std::fs::canonicalize(&mount).unwrap().file_name().unwrap(),
        "node_modules"
    );
}

/// R1.31: a build without a revision uses the default start, so with only
/// a `master` bookmark it builds `master`'s lockfile, not the one in the
/// root checkout's newer change.
#[tokio::test(flavor = "multi_thread")]
async fn store_build_without_rev_uses_master() {
    let (daemon, repo) = fixture().await;
    let root = repo.root.clone();
    tokio::task::spawn_blocking(move || {
        pollster::block_on(async {
            let mut checkout = Repo::open(&root, &test_settings()).await.unwrap();
            assert!(
                checkout
                    .resolve_bookmark("main", None)
                    .await
                    .unwrap()
                    .is_none(),
                "the fixture has no `main`, which would win over `master`"
            );
            let master = checkout.wc_change().unwrap().commit_id;
            // `jj new`, then `jj bookmark set master -r @-`.
            let moved = checkout
                .move_pointer("default", StartPoint::NewOn(master.clone()), "new")
                .await
                .unwrap();
            checkout.check_out_tree(&moved.wc.commit_id).await.unwrap();
            drop(checkout);
            set_bookmark(&root, "master", &master);
        });
    })
    .await
    .unwrap();
    commit_at_root(&repo.root, &[("deps.lock", "v2\n")]).await;

    let done = build(&daemon, &repo).await;
    assert!(done.sealed, "{done:?}");
    assert_eq!(done.lockfile_hash, key_hash(), "master's lockfile");
    assert_eq!(
        std::fs::read_to_string(Path::new(&done.path).join("manifest")).unwrap(),
        LOCKFILE
    );
}

/// Sets a local bookmark the way `jj bookmark set` does.
fn set_bookmark(root: &Path, name: &str, commit: &CommitIdHex) {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;
    use jj_lib::workspace::Workspace;

    let workspace = Workspace::load(
        &test_settings(),
        root,
        &default_backend_factories(),
        &default_working_copy_factories(),
    )
    .unwrap();
    let repo = pollster::block_on(workspace.repo_loader().load_at_head()).unwrap();
    let mut tx = repo.start_transaction();
    tx.repo_mut().set_local_bookmark_target(
        RefName::new(name),
        RefTarget::normal(CommitId::try_from_hex(commit.as_str()).unwrap()),
    );
    pollster::block_on(tx.commit("bookmark set")).unwrap();
}

// RX.01 test 8 (S-03): `[store] allow_builds = false` refuses every build,
// before anything is written, and a create still names the build as the
// fix.
#[tokio::test(flavor = "multi_thread")]
async fn build_is_refused_when_builds_are_off() {
    let (daemon, repo) = fixture_with(|config| config.file.store.allow_builds = false).await;
    let built = build_with(&daemon, &repo, Duration::from_mins(5)).await;
    let error = built.error.expect("refused");
    assert_eq!(error.kind(), ErrorKind::PermissionDenied, "{error}");
    let hint = error.advice().unwrap().hint;
    assert!(hint.contains("[store] allow_builds"), "{hint}");
    assert!(built.started.is_empty() && built.lines.is_empty());
    let stores = daemon.state_dir().join("stores");
    assert!(
        std::fs::read_dir(&stores).map_or(true, |mut entries| entries.next().is_none()),
        "nothing under {}",
        stores.display()
    );
    let error = create(&daemon, &repo, create_request::MissingStores::Fail)
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Unavailable, "{error}");
    assert_eq!(
        error.advice().unwrap().next[..3],
        ["reef", "store", "build"]
    );
}

// RX.01 test 9 (S-03): the first event says what the daemon runs, and the
// daemon logs it.
#[tokio::test(flavor = "multi_thread")]
async fn build_announces_its_command() {
    logs();
    let (daemon, repo) = fixture().await;
    let spec = format!("{SPEC}[store.env]\nNODE_ENV = \"production\"\nA = \"1\"\n");
    commit_at_root(&repo.root, &[("reef.toml", &spec)]).await;
    let built = build_with(&daemon, &repo, Duration::from_mins(5)).await;
    assert!(built.error.is_none(), "{:?}", built.error);
    assert_eq!(built.started.len(), 1, "{:?}", built.started);
    let (lines_before, started) = &built.started[0];
    assert_eq!(*lines_before, 0, "before any output line");
    assert_eq!(started.argv[..2], ["sh", "-c"]);
    assert!(
        started.argv[2].contains("REEF_STORE_DIR"),
        "{:?}",
        started.argv
    );
    assert_eq!(started.env, ["A=1", "NODE_ENV=production"]);
    assert_eq!(started.rev.len(), 40, "{}", started.rev);
    assert!(
        logs().has_line(&[
            "INFO",
            "running the store's build command",
            "argv=[\"sh\", \"-c\"",
            "env_keys=[\"A\", \"NODE_ENV\"]",
        ]),
        "no info line with the argv"
    );
}
